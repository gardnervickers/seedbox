//! Fault editor driven by the server's live JSON schemas.
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::style::Modifier;
use ratatui::{
    layout::Rect,
    style::{Color, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Clear, Paragraph},
    Frame,
};
use seedbox_api::FaultCatalogue;

const ICE: Color = Color::Rgb(120, 200, 255);
const TEXT: Color = Color::Rgb(200, 211, 223);
const DIM: Color = Color::Rgb(131, 148, 170);
const FAINT: Color = Color::Rgb(93, 112, 137);
const GREEN: Color = Color::Rgb(89, 211, 140);
const SELECTED: Color = Color::Rgb(19, 36, 58);
use serde_json::{Map, Value};

#[derive(Debug, PartialEq)]
pub(super) enum FormOutcome {
    None,
    Cancel,
    Submit(Value),
}

#[derive(Debug)]
struct Field {
    path: Vec<String>,
    schema: Value,
    required: bool,
    text: String,
    choice: usize,
}

/// What the console knows when the palette opens, used only for defaults,
/// heal shortcuts and the map preview. The schema still decides validity.
#[derive(Clone, Debug, Default)]
pub(super) struct FormContext {
    pub selected: Option<String>,
    /// Every handle the world has used, so new ones stay unique.
    pub handles: Vec<String>,
    /// Active faults a heal can close: handle and a short description.
    pub active: Vec<(String, String)>,
}

#[derive(Clone, Debug, PartialEq)]
enum Entry {
    Heal(String, String),
    SpawnCopy(String),
    Fault(usize),
}

pub(super) struct FaultForm {
    catalogue: FaultCatalogue,
    context: FormContext,
    /// The catalogue fault being edited.
    selected: usize,
    /// The highlighted picker entry.
    cursor: usize,
    filter: String,
    editing: bool,
    fields: Vec<Field>,
    focus: usize,
    error: Option<String>,
    preview_scroll: std::cell::Cell<usize>,
    preview_request: Option<(Value, u64)>,
    preview: Option<Result<seedbox_api::FaultPreview, String>>,
    hitboxes: std::cell::RefCell<Vec<(Rect, usize)>>,
}

impl FaultForm {
    pub(super) fn new(catalogue: FaultCatalogue) -> Self {
        Self {
            catalogue,
            context: FormContext::default(),
            selected: 0,
            cursor: 0,
            filter: String::new(),
            editing: false,
            fields: Vec::new(),
            focus: 0,
            error: None,
            preview_scroll: std::cell::Cell::new(0),
            preview_request: None,
            preview: None,
            hitboxes: Default::default(),
        }
    }

    pub(super) fn with_context(mut self, context: FormContext) -> Self {
        self.context = context;
        self
    }

    /// Only the server determines map targets; partial text is not evidence.
    pub(super) fn targets(&self) -> Vec<String> {
        self.preview
            .as_ref()
            .and_then(|p| p.as_ref().ok())
            .map(|p| p.machines.clone())
            .unwrap_or_default()
    }

    pub(super) fn targets_link(&self, from: &str, to: &str) -> bool {
        self.preview
            .as_ref()
            .and_then(|p| p.as_ref().ok())
            .is_some_and(|p| {
                p.links
                    .iter()
                    .any(|link| link.from == from && link.to == to)
            })
    }

    pub(super) fn request_preview(&mut self, revision: u64) -> Option<Value> {
        let action = self.editing.then(|| self.value().ok()).flatten();
        let next = action.map(|action| (action, revision));
        if self.preview_request == next {
            return None;
        }
        self.preview = None;
        self.preview_scroll.set(0);
        self.preview_request = next.clone();
        next.map(|(action, _)| action)
    }

    pub(super) fn accept_preview(
        &mut self,
        action: Value,
        revision: u64,
        result: Result<seedbox_api::FaultPreview, String>,
    ) {
        if self.preview_request.as_ref() == Some(&(action, revision)) {
            if self.error.as_deref()
                == Some("Review the effect preview before pressing Enter to submit.")
            {
                self.error = None;
            }
            self.preview = Some(result);
        }
    }

    pub(super) fn preview_ready(&self, action: &Value) -> bool {
        self.preview_request
            .as_ref()
            .is_some_and(|(requested, _)| requested == action)
            && matches!(self.preview, Some(Ok(_)))
    }

    fn heal_schema(&self) -> Option<&Value> {
        self.catalogue
            .faults
            .iter()
            .find(|fault| fault.name == "heal")
            .map(|fault| &fault.schema)
    }

    fn entries(&self) -> Vec<Entry> {
        let filter = self.filter.to_lowercase();
        let matches = |text: &str| filter.is_empty() || text.to_lowercase().contains(&filter);
        let mut out = Vec::new();
        if let Some(source) = &self.context.selected {
            if self.catalogue.faults.iter().any(|fault| {
                fault.name == "spawn"
                    && choices(&fault.schema["properties"]["from"])
                        .iter()
                        .any(|choice| choice.as_str() == Some(source))
            }) && matches(&format!("spawn a copy {source}"))
            {
                out.push(Entry::SpawnCopy(source.clone()));
            }
        }
        if self.heal_schema().is_some() {
            for (handle, description) in &self.context.active {
                if matches(&format!("heal {handle} {description}")) {
                    out.push(Entry::Heal(handle.clone(), description.clone()));
                }
            }
        }
        for (index, fault) in self.catalogue.faults.iter().enumerate() {
            if matches(&fault.name) || matches(&fault.title) || matches(fault.category.label()) {
                out.push(Entry::Fault(index));
            }
        }
        out
    }

    /// The immediate spawn action must be valid against the current catalogue.
    pub(super) fn quick_spawn_action(&self) -> Option<Value> {
        let source = self.context.selected.as_ref()?;
        let schema = &self
            .catalogue
            .faults
            .iter()
            .find(|fault| fault.name == "spawn")?
            .schema;
        let action = serde_json::json!({
            "action": "spawn",
            "from": source,
            "machine": self.fresh_machine(source),
        });
        validate(schema, &action, "spawn").ok()?;
        Some(action)
    }

    /// Follow the oracle's `source-2` convention, within the DNS-label limit.
    fn fresh_machine(&self, source: &str) -> String {
        (2..)
            .map(|number| {
                let suffix = format!("-{number}");
                let prefix: String = source
                    .chars()
                    .take(63usize.saturating_sub(suffix.len()))
                    .collect();
                format!("{}{suffix}", prefix.trim_end_matches('-'))
            })
            .find(|name| !self.context.machines.contains(name))
            .unwrap_or_else(|| "machine-2".into())
    }

    /// A handle not yet used in this world, from the action's initial.
    fn fresh_handle(&self, action: &str) -> String {
        let initial = action.chars().next().unwrap_or('h').to_ascii_lowercase();
        (1..)
            .map(|n| format!("{initial}{n}"))
            .find(|candidate| !self.context.handles.contains(candidate))
            .unwrap_or_else(|| "h1".into())
    }

    /// Keep the editor and entered parameters available after server rejection.
    pub(super) fn set_error(&mut self, message: impl Into<String>) {
        self.error = Some(message.into());
    }

    pub(super) fn update_catalogue(&mut self, catalogue: FaultCatalogue) {
        let name = self
            .catalogue
            .faults
            .get(self.selected)
            .map(|f| f.name.clone());
        let submit_focused = self.focus == self.fields.len();
        let focused_path = self.fields.get(self.focus).map(|field| field.path.clone());
        let old_fields = std::mem::take(&mut self.fields);
        self.catalogue = catalogue;
        if let Some(index) = self
            .catalogue
            .faults
            .iter()
            .position(|f| Some(&f.name) == name.as_ref())
        {
            self.selected = index;
            if self.editing {
                self.build_fields();
                for field in &mut self.fields {
                    if let Some(old) = old_fields.iter().find(|old| old.path == field.path) {
                        field.text.clone_from(&old.text);
                        if let Some(selected) = choices(&old.schema).get(old.choice) {
                            field.choice = choices(&field.schema)
                                .iter()
                                .position(|choice| choice == selected)
                                .unwrap_or(0);
                        }
                    }
                }
                self.focus = if submit_focused {
                    self.fields.len()
                } else {
                    self.fields
                        .iter()
                        .position(|field| Some(&field.path) == focused_path.as_ref())
                        .unwrap_or_else(|| self.focus.min(self.fields.len()))
                };
            }
        } else {
            self.selected = 0;
            self.editing = false;
            self.error = name.map(|_| "This fault is no longer available.".to_owned());
        }
    }

    fn build_fields(&mut self) {
        self.fields.clear();
        let Some(fault) = self.catalogue.faults.get(self.selected) else {
            return;
        };
        flatten(&fault.schema, &mut Vec::new(), true, None, &mut self.fields);
        let name = fault.name.clone();
        let fresh = self.fresh_handle(&name);
        let machine = self
            .context
            .selected
            .as_ref()
            .map(|source| self.fresh_machine(source));
        for field in &mut self.fields {
            if !field.text.is_empty() {
                continue;
            }
            let options = choices(&field.schema);
            let array = field.schema["type"] == "array";
            if let Some(selected) = &self.context.selected {
                if let Some(index) = options.iter().position(|c| c.as_str() == Some(selected)) {
                    field.choice = index;
                    if !array {
                        field.text = selected.clone();
                    }
                    continue;
                }
            }
            if name == "spawn" && field.path == ["machine"] {
                if let Some(machine) = &machine {
                    field.text.clone_from(machine);
                }
            }
            let is_handle = field.path.last().is_some_and(|last| last == "handle");
            if is_handle && options.is_empty() && name != "heal" && field.schema["type"] == "string"
            {
                field.text.clone_from(&fresh);
            }
        }
    }

    pub(super) fn input(&mut self, key: KeyEvent) -> FormOutcome {
        if key.code == KeyCode::Esc {
            return FormOutcome::Cancel;
        }
        if !self.editing {
            let entries = self.entries();
            let count = entries.len();
            match key.code {
                KeyCode::Up if count > 0 => self.cursor = (self.cursor + count - 1) % count,
                KeyCode::Down | KeyCode::Tab if count > 0 => {
                    self.cursor = (self.cursor + 1) % count
                }
                KeyCode::Enter if count > 0 => match &entries[self.cursor.min(count - 1)] {
                    Entry::SpawnCopy(_) => {
                        return self
                            .quick_spawn_action()
                            .map_or(FormOutcome::None, FormOutcome::Submit);
                    }
                    Entry::Fault(index) => {
                        self.selected = *index;
                        self.editing = true;
                        self.focus = 0;
                        self.error = None;
                        self.build_fields();
                    }
                    Entry::Heal(handle, _) => {
                        if let Some(index) = self
                            .catalogue
                            .faults
                            .iter()
                            .position(|fault| fault.name == "heal")
                        {
                            self.selected = index;
                            self.editing = true;
                            self.error = None;
                            self.build_fields();
                            if let Some(field) = self
                                .fields
                                .iter_mut()
                                .find(|field| field.path == ["handle"])
                            {
                                field.text = handle.clone();
                            }
                            self.focus = self.fields.len();
                        }
                    }
                },
                KeyCode::Backspace => {
                    self.filter.pop();
                    self.cursor = 0;
                }
                KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    self.filter.clear();
                    self.cursor = 0;
                }
                KeyCode::Char(c)
                    if !key
                        .modifiers
                        .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
                        && self.filter.len() < 40 =>
                {
                    self.filter.push(c);
                    self.cursor = 0;
                }
                _ => {}
            }
            return FormOutcome::None;
        }
        if self.focus == self.fields.len() {
            match key.code {
                KeyCode::Char('r') if matches!(self.preview, Some(Err(_))) => {
                    self.preview_request = None;
                    return FormOutcome::None;
                }
                KeyCode::PageDown => {
                    self.preview_scroll
                        .set(self.preview_scroll.get().saturating_add(4));
                    return FormOutcome::None;
                }
                KeyCode::PageUp => {
                    self.preview_scroll
                        .set(self.preview_scroll.get().saturating_sub(4));
                    return FormOutcome::None;
                }
                _ => {}
            }
        }
        if key.code == KeyCode::Enter && self.focus == self.fields.len() {
            return match self.value() {
                Ok(value) => FormOutcome::Submit(value),
                Err(error) => {
                    self.error = Some(error);
                    FormOutcome::None
                }
            };
        }
        let count = self.fields.len() + 1;
        if key.code == KeyCode::Char('u') && key.modifiers.contains(KeyModifiers::CONTROL) {
            if let Some(field) = self.fields.get_mut(self.focus) {
                field.text.clear();
            }
            self.error = None;
            return FormOutcome::None;
        }
        match key.code {
            KeyCode::Tab | KeyCode::Down if count > 0 => self.focus = (self.focus + 1) % count,
            KeyCode::BackTab | KeyCode::Up if count > 0 => {
                self.focus = (self.focus + count - 1) % count
            }
            KeyCode::Backspace => {
                if let Some(field) = self.fields.get_mut(self.focus) {
                    field.text.pop();
                }
            }
            _ => {
                if let Some(field) = self.fields.get_mut(self.focus) {
                    let choices = choices(&field.schema);
                    let array = field.schema["type"] == "array";
                    match key.code {
                        KeyCode::Left | KeyCode::Right | KeyCode::Enter if !choices.is_empty() => {
                            if key.code == KeyCode::Enter {
                                if !array {
                                    field.text = display_value(&choices[field.choice]);
                                }
                                self.focus = (self.focus + 1) % count;
                            } else {
                                let offset = if key.code == KeyCode::Left {
                                    choices.len() - 1
                                } else {
                                    1
                                };
                                // The first arrow picks the shown choice; later ones move.
                                if array || !field.text.is_empty() {
                                    field.choice = (field.choice + offset) % choices.len();
                                }
                                if !array {
                                    field.text = display_value(&choices[field.choice]);
                                }
                            }
                        }
                        KeyCode::Enter => self.focus = (self.focus + 1) % count,
                        KeyCode::Char(' ') if array && !choices.is_empty() => {
                            let choice = display_value(&choices[field.choice]);
                            let mut selected: Vec<String> = field
                                .text
                                .split(',')
                                .map(str::trim)
                                .filter(|s| !s.is_empty())
                                .map(str::to_owned)
                                .collect();
                            if selected.contains(&choice) {
                                selected.retain(|s| s != &choice);
                            } else {
                                selected.push(choice);
                            }
                            field.text = selected.join(", ");
                        }
                        KeyCode::Char(c)
                            if !key
                                .modifiers
                                .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
                        {
                            field.text.push(c)
                        }
                        _ => {}
                    }
                }
            }
        }
        FormOutcome::None
    }

    fn value(&self) -> Result<Value, String> {
        let fault = self
            .catalogue
            .faults
            .get(self.selected)
            .ok_or("No faults available.")?;
        let mut result = Value::Object(Map::new());
        apply_constants(&fault.schema, &mut result);
        for field in &self.fields {
            if field.text.is_empty() {
                continue;
            }
            let value = parse(&field.schema, &field.text)
                .map_err(|e| format!("{}: {e}", field.path.join(".")))?;
            insert(&mut result, &field.path, value);
        }
        validate(&fault.schema, &result, "fault")?;
        Ok(result)
    }

    pub(super) fn mouse(&mut self, event: MouseEvent) -> Option<KeyEvent> {
        let target = self
            .hitboxes
            .borrow()
            .iter()
            .find(|(rect, _)| rect.contains((event.column, event.row).into()))
            .map(|(_, index)| *index);
        let index = target?;
        match event.kind {
            MouseEventKind::ScrollUp => Some(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE)),
            MouseEventKind::ScrollDown => Some(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE)),
            MouseEventKind::Down(MouseButton::Left) => {
                if self.editing {
                    self.focus = index;
                    (index == self.fields.len())
                        .then(|| KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
                } else {
                    self.cursor = index;
                    Some(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
                }
            }
            _ => None,
        }
    }

    pub(super) fn draw(&self, frame: &mut Frame, area: Rect, map_bottom: Option<u16>) {
        let width = area.width.saturating_sub(4).min(92);
        // Sit below the network map when there is room for the form, so every
        // previewed machine and link stays visible; otherwise in the lower
        // part of tall terminals; size to content.
        const ROOM: u16 = 18;
        let top = match map_bottom {
            Some(bottom) if area.y + area.height > bottom + ROOM => bottom,
            _ if area.height >= 34 => area.y + area.height * 2 / 5,
            _ => area.y + 1,
        };
        let height = (area.y + area.height).saturating_sub(top + 1).min(28);
        let mut modal = Rect::new(area.x + (area.width - width) / 2, top, width, height);
        let text_width = usize::from(width.saturating_sub(4));
        self.hitboxes.borrow_mut().clear();
        let mut lines = Vec::new();
        if !self.editing {
            lines.push(Line::from(vec![
                Span::styled(": ", Style::default().fg(ICE).add_modifier(Modifier::BOLD)),
                Span::styled(self.filter.clone(), Style::default().fg(Color::White)),
                Span::styled("▏", Style::default().fg(ICE)),
                Span::styled(
                    if self.filter.is_empty() {
                        "  type to filter · ↑↓ choose · ⏎ select · esc cancel"
                    } else {
                        ""
                    },
                    Style::default().fg(FAINT),
                ),
            ]));
            let entries = self.entries();
            if entries.is_empty() {
                lines.push(Line::styled(
                    "  nothing matches",
                    Style::default().fg(FAINT),
                ));
            }
            let visible = usize::from(height.saturating_sub(8)).max(1);
            let start = self.cursor.saturating_sub(visible - 1);
            let mut section = "";
            for (index, entry) in entries.iter().enumerate().skip(start).take(visible) {
                let heading = match entry {
                    Entry::SpawnCopy(_) => "QUICK ACTION",
                    Entry::Heal(..) => "HEAL",
                    Entry::Fault(_) => "INJECT",
                };
                if heading != section {
                    section = heading;
                    lines.push(Line::styled(
                        format!(" {heading}"),
                        Style::default().fg(FAINT).add_modifier(Modifier::BOLD),
                    ));
                }
                self.hitboxes.borrow_mut().push((
                    Rect::new(
                        modal.x + 1,
                        modal.y + 1 + lines.len() as u16,
                        modal.width.saturating_sub(2),
                        1,
                    ),
                    index,
                ));
                let on = index == self.cursor;
                let marker = if on { "›" } else { " " };
                let line = match entry {
                    Entry::SpawnCopy(source) => Line::from(vec![
                        Span::styled(
                            format!("{marker} ✚ Spawn a copy of {source}  "),
                            Style::default().fg(if on { ICE } else { TEXT }),
                        ),
                        Span::styled(
                            format!("→ {} · Enter to spawn", self.fresh_machine(source)),
                            Style::default().fg(DIM),
                        ),
                    ]),
                    Entry::Heal(handle, description) => Line::from(vec![
                        Span::styled(
                            format!("{marker} ✚ heal {handle}  "),
                            Style::default().fg(GREEN),
                        ),
                        Span::styled(description.clone(), Style::default().fg(DIM)),
                    ]),
                    Entry::Fault(i) => {
                        let fault = &self.catalogue.faults[*i];
                        Line::from(vec![
                            Span::styled(
                                format!("{marker}   {:<26}", fault.title),
                                Style::default().fg(if on { ICE } else { TEXT }),
                            ),
                            Span::styled(fault.category.label(), Style::default().fg(FAINT)),
                        ])
                    }
                };
                lines.push(if on {
                    line.style(Style::default().bg(SELECTED).add_modifier(Modifier::BOLD))
                } else {
                    line
                });
            }
            match entries.get(self.cursor) {
                Some(Entry::Fault(i)) => {
                    lines.push(Line::raw(""));
                    for text in wrap_text(&self.catalogue.faults[*i].description, text_width, 3) {
                        lines.push(Line::styled(text, Style::default().fg(DIM)));
                    }
                }
                Some(Entry::SpawnCopy(_)) => {
                    lines.push(Line::styled(
                        "Use Spawn machine below to customize the name, env or argv.",
                        Style::default().fg(DIM),
                    ));
                }
                _ => {}
            }
        } else if let Some(fault) = self.catalogue.faults.get(self.selected) {
            lines.push(Line::styled(
                format!("{} · {}", fault.category.label(), fault.title),
                Style::default()
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD),
            ));
            for text in wrap_text(&fault.description, text_width, 2) {
                lines.push(Line::styled(text, Style::default().fg(DIM)));
            }
            lines.push(Line::styled(
                "⇥ next field · ← → choose · space toggles a list · ⏎ next / submit · esc cancel",
                Style::default().fg(FAINT),
            ));
            let visible = if self.focus == self.fields.len() {
                0
            } else {
                usize::from(height.saturating_sub(18)).max(1)
            };
            let start = self.focus.saturating_sub(visible.saturating_sub(1));
            let name_width = self
                .fields
                .iter()
                .map(|f| f.path.join(".").chars().count() + if f.required { 2 } else { 0 })
                .max()
                .unwrap_or(10)
                .max(10)
                .min(usize::from(width) / 2)
                + 2;
            for (index, field) in self.fields.iter().enumerate().skip(start).take(visible) {
                let unit = field.schema["x-unit"].as_str().unwrap_or("");
                let options = choices(&field.schema);
                let on = index == self.focus;
                let (value, value_color) = if field.schema["type"] == "array" {
                    if field.text.is_empty() {
                        (
                            format!(
                                "none · space adds {}",
                                options
                                    .get(field.choice)
                                    .map(display_value)
                                    .unwrap_or_default()
                            ),
                            FAINT,
                        )
                    } else {
                        (field.text.clone(), Color::White)
                    }
                } else if field.text.is_empty() && !options.is_empty() {
                    ("← → choose".to_string(), FAINT)
                } else if field.text.is_empty() {
                    ("unset".to_string(), FAINT)
                } else {
                    (
                        format!(
                            "{}{}",
                            field.text,
                            // Bare numbers are in the canonical unit; text that
                            // already carries a unit is shown as typed.
                            if unit.is_empty() || field.text.parse::<f64>().is_err() {
                                String::new()
                            } else {
                                format!(" {unit}")
                            }
                        ),
                        Color::White,
                    )
                };
                let name = format!(
                    "{}{}",
                    field.path.join("."),
                    if field.required { " *" } else { "" }
                );
                self.hitboxes.borrow_mut().push((
                    Rect::new(
                        modal.x + 1,
                        modal.y + 1 + lines.len() as u16,
                        modal.width.saturating_sub(2),
                        1,
                    ),
                    index,
                ));
                let line = Line::from(vec![
                    Span::styled(if on { "› " } else { "  " }, Style::default().fg(ICE)),
                    Span::styled(
                        format!("{name:<name_width$}"),
                        Style::default().fg(if on { ICE } else { DIM }),
                    ),
                    Span::styled(value, Style::default().fg(value_color)),
                ]);
                lines.push(if on {
                    line.style(Style::default().bg(SELECTED))
                } else {
                    line
                });
            }
            if let Some(field) = self.fields.get(self.focus) {
                if let Some(description) = field.schema["description"].as_str() {
                    for text in wrap_text(description, text_width, 2) {
                        lines.push(Line::styled(text, Style::default().fg(DIM)));
                    }
                }
                if field.schema["type"] == "object" {
                    lines.push(Line::styled(
                        "Enter a JSON object, for example {\"MODE\":\"test\"}.",
                        Style::default().fg(DIM),
                    ));
                } else if field.path == ["argv"] && field.schema["type"] == "array" {
                    lines.push(Line::styled(
                        "Use JSON for exact arguments, for example [\"--key=one,two\"].",
                        Style::default().fg(DIM),
                    ));
                }
                if let Some(example) = field.schema["x-input-example"].as_str() {
                    let units = field.schema["x-input-units"]
                        .as_object()
                        .map(|units| units.keys().cloned().collect::<Vec<_>>().join(", "))
                        .unwrap_or_default();
                    lines.push(Line::from(format!(
                        "Example: {example} · Units: {units} · bare numbers use {}",
                        field.schema["x-unit"].as_str().unwrap_or("canonical units")
                    )));
                }
                let options = choices(&field.schema);
                if let Some(choice) = options.get(field.choice) {
                    lines.push(Line::from(vec![
                        Span::raw("Choice: "),
                        Span::raw(format!(
                            "{} ({}/{}) · ←/→ browse{}",
                            display_value(choice),
                            field.choice + 1,
                            options.len(),
                            if field.schema["type"] == "array" {
                                " · Space toggle"
                            } else {
                                " · Enter confirm and next"
                            }
                        )),
                    ]));
                }
                let constraints = ["minimum", "maximum", "minItems", "maxItems"]
                    .iter()
                    .filter_map(|key| field.schema.get(*key).map(|v| format!("{key}: {v}")))
                    .collect::<Vec<_>>()
                    .join(" · ");
                if !constraints.is_empty() {
                    lines.push(Line::from(constraints));
                }
            }
        }
        if self.editing {
            let reviewing = self.focus == self.fields.len();
            let budget = usize::from(height).saturating_sub(lines.len() + 5);
            let mut explanation = Vec::new();
            match &self.preview {
                Some(Ok(preview)) => {
                    explanation.push(format!("{} effect preview", preview.category.label()));
                    explanation.push(format!("Expected targets: {}", preview.machines.join(", ")));
                    if !preview.links.is_empty() {
                        explanation.push(format!(
                            "Directed links: {}",
                            preview
                                .links
                                .iter()
                                .map(|l| format!("{} → {}", l.from, l.to))
                                .collect::<Vec<_>>()
                                .join(", ")
                        ));
                    }
                    explanation.push(preview.expected_effect.clone());
                    if reviewing {
                        explanation.extend(preview.observations.clone());
                        explanation.extend(
                            preview
                                .preconditions
                                .iter()
                                .map(|text| format!("Requires: {text}")),
                        );
                        explanation.extend(preview.limitations.clone());
                        explanation.push(format!(
                            "Observed revision {} at {} ns",
                            preview.revision, preview.simulated_time_ns
                        ));
                    } else {
                        explanation.push(
                            "Expected effects; submission rechecks state. Tab to Submit to review."
                                .into(),
                        );
                    }
                }
                Some(Err(error)) => explanation.push(format!(
                    "Preview unavailable: {error}. Focus Submit and press r to retry."
                )),
                None if self.preview_request.is_some() => {
                    explanation.push("Loading effect preview…".into())
                }
                None => explanation.push("Complete the fields to preview expected effects.".into()),
            }
            let wrapped: Vec<_> = explanation
                .iter()
                .flat_map(|text| wrap_text(text, text_width, usize::MAX))
                .collect();
            let overflow = wrapped.len() > budget;
            let visible = budget.saturating_sub(usize::from(overflow));
            let start = if reviewing {
                self.preview_scroll
                    .get()
                    .min(wrapped.len().saturating_sub(visible.max(1)))
            } else {
                0
            };
            self.preview_scroll.set(start);
            for text in wrapped.iter().skip(start).take(visible) {
                lines.push(Line::styled(text.clone(), Style::default().fg(ICE)));
            }
            if overflow && budget > 0 {
                lines.push(Line::styled(
                    if reviewing {
                        "PgUp/PgDn scroll effect preview"
                    } else {
                        "Tab to Submit to review the full preview"
                    },
                    Style::default().fg(DIM),
                ));
            }
        }
        if let Some(error) = &self.error {
            lines.push(Line::styled(
                error.clone(),
                Style::default().fg(Color::Rgb(255, 90, 106)),
            ));
        }
        let needed = lines.len() as u16 + 2 + if self.editing { 2 } else { 0 };
        modal.height = needed.clamp(8.min(height), height);
        frame.render_widget(Clear, modal);
        frame.render_widget(
            Paragraph::new(lines).block(
                Block::bordered()
                    .border_type(BorderType::Rounded)
                    .border_style(Style::default().fg(ICE))
                    .title(Span::styled(
                        " inject ",
                        Style::default().fg(ICE).add_modifier(Modifier::BOLD),
                    ))
                    .title(
                        Line::styled(" playback held while open ", Style::default().fg(FAINT))
                            .right_aligned(),
                    )
                    .style(Style::default().bg(Color::Rgb(11, 18, 28))),
            ),
            modal,
        );
        if self.editing {
            let button = Rect::new(
                modal.x + 1,
                modal.y + modal.height.saturating_sub(2),
                modal.width.saturating_sub(2),
                1,
            );
            self.hitboxes.borrow_mut().push((button, self.fields.len()));
            frame.render_widget(
                Paragraph::new(if self.focus == self.fields.len() {
                    "› [ Submit fault ]  Enter sends · Esc cancels"
                } else {
                    "  [ Submit fault ]  Tab to focus · Enter sends"
                })
                .style(if self.focus == self.fields.len() {
                    Style::default().fg(Color::Rgb(8, 12, 18)).bg(ICE)
                } else {
                    Style::default().fg(ICE)
                }),
                button,
            );
        }
    }
}

/// Word-wrap `text` into at most `rows` lines of `width`, ending with an
/// ellipsis when it does not fit.
fn wrap_text(text: &str, width: usize, rows: usize) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut current = String::new();
    for word in text.split_whitespace() {
        if !current.is_empty() && current.chars().count() + 1 + word.chars().count() > width {
            out.push(std::mem::take(&mut current));
        }
        if !current.is_empty() {
            current.push(' ');
        }
        current.push_str(word);
    }
    if !current.is_empty() {
        out.push(current);
    }
    if out.len() > rows {
        out.truncate(rows);
        if let Some(last) = out.last_mut() {
            *last = compact(last, width.saturating_sub(1));
            if !last.ends_with('…') {
                last.push('…');
            }
        }
    }
    out
}

fn compact(text: &str, limit: usize) -> String {
    let mut result: String = text.chars().take(limit).collect();
    if text.chars().count() > limit {
        result.push('…');
    }
    result
}

fn flatten(
    schema: &Value,
    path: &mut Vec<String>,
    required: bool,
    inherited_default: Option<&Value>,
    fields: &mut Vec<Field>,
) {
    if schema.get("const").is_some() {
        return;
    }
    let default = schema.get("default").or(inherited_default);
    if schema["type"] == "object" {
        if let Some(properties) = schema["properties"].as_object() {
            for (name, child) in properties {
                path.push(name.clone());
                let required = schema["required"]
                    .as_array()
                    .is_some_and(|names| names.iter().any(|v| v.as_str() == Some(name)));
                flatten(
                    child,
                    path,
                    required,
                    default.and_then(|v| v.get(name)),
                    fields,
                );
                path.pop();
            }
            if !properties.is_empty() {
                return;
            }
        }
        // An object with dynamic keys has no child fields to render; edit it
        // as JSON instead of silently losing its values.
        if !schema["additionalProperties"].is_object() && schema["additionalProperties"] != true {
            return;
        }
    }
    fields.push(Field {
        path: path.clone(),
        schema: schema.clone(),
        required,
        text: default
            .map(|value| display_field_value(schema, value))
            .unwrap_or_default(),
        choice: default
            .and_then(|value| choices(schema).iter().position(|choice| choice == value))
            .unwrap_or(0),
    });
}

fn choices(schema: &Value) -> Vec<Value> {
    let schema = if schema["type"] == "array" {
        &schema["items"]
    } else {
        schema
    };
    schema["enum"].as_array().cloned().unwrap_or_else(|| {
        if schema["type"] == "boolean" {
            vec![Value::Bool(false), Value::Bool(true)]
        } else {
            Vec::new()
        }
    })
}

fn display_value(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        Value::Array(items) => items
            .iter()
            .map(display_value)
            .collect::<Vec<_>>()
            .join(", "),
        _ => value.to_string(),
    }
}

/// Render defaults with schema-selected units without floating point rounding.
fn display_field_value(schema: &Value, value: &Value) -> String {
    let Some(unit) = schema["x-display-unit"].as_str() else {
        return display_value(value);
    };
    let Some(factor) = schema["x-input-units"][unit].as_u64().filter(|n| *n > 0) else {
        return display_value(value);
    };
    let Some(number) = integer_value(value) else {
        return display_value(value);
    };
    let digits = factor.ilog10();
    if 10u64.pow(digits) != factor {
        return display_value(value);
    }
    let absolute = number.unsigned_abs();
    let whole = absolute / u128::from(factor);
    let remainder = absolute % u128::from(factor);
    let fraction = if remainder == 0 {
        String::new()
    } else {
        format!(".{:0width$}", remainder, width = digits as usize)
            .trim_end_matches('0')
            .to_owned()
    };
    format!(
        "{}{whole}{fraction}{unit}",
        if number < 0 { "-" } else { "" }
    )
}

fn integer_value(value: &Value) -> Option<i128> {
    value
        .as_i64()
        .map(i128::from)
        .or_else(|| value.as_u64().map(i128::from))
}

/// Convert decimal input exactly; fractions of a canonical integer are errors.
fn parse_quantity(schema: &Value, text: &str) -> Result<Value, String> {
    let units = schema["x-input-units"]
        .as_object()
        .ok_or("No input units supplied.")?;
    let text = text.trim();
    if let Ok(value) = serde_json::from_str::<Value>(text) {
        if integer_value(&value).is_some() {
            return Ok(value);
        }
    }
    let split = text
        .char_indices()
        .find(|(_, c)| !c.is_ascii_digit() && !matches!(c, '+' | '-' | '.'))
        .map(|(i, _)| i)
        .unwrap_or(text.len());
    let (amount, suffix) = text.split_at(split);
    let suffix = suffix.trim();
    let canonical = schema["x-unit"].as_str().unwrap_or("canonical units");
    let factor = if suffix.is_empty() {
        1
    } else {
        units
            .get(suffix)
            .and_then(Value::as_u64)
            .filter(|n| *n > 0)
            .ok_or_else(|| {
                format!(
                    "Unknown unit {suffix:?}; use {} (for example {}).",
                    units.keys().cloned().collect::<Vec<_>>().join(", "),
                    schema["x-input-example"]
                        .as_str()
                        .unwrap_or("a number with a unit")
                )
            })?
    };
    let negative = amount.starts_with('-');
    let unsigned = amount
        .strip_prefix('-')
        .or_else(|| amount.strip_prefix('+'))
        .unwrap_or(amount);
    let mut parts = unsigned.split('.');
    let whole = parts.next().unwrap_or_default();
    let fraction = parts.next().unwrap_or_default().trim_end_matches('0');
    if parts.next().is_some()
        || (whole.is_empty() && fraction.is_empty())
        || !whole
            .bytes()
            .chain(fraction.bytes())
            .all(|b| b.is_ascii_digit())
    {
        return Err("Enter a decimal number followed by a supported unit.".into());
    }
    let precision = u32::try_from(fraction.len()).map_err(|_| "Too many decimal places.")?;
    let scale = 10i128
        .checked_pow(precision)
        .ok_or("Too many decimal places.")?;
    let digits = format!("{whole}{fraction}");
    let numerator = digits
        .parse::<i128>()
        .map_err(|_| "Number is too large.")?
        .checked_mul(i128::from(factor))
        .ok_or("Number is too large.")?;
    if numerator % scale != 0 {
        return Err(format!(
            "Amount must resolve to a whole number of {canonical}."
        ));
    }
    let number = (numerator / scale) * if negative { -1 } else { 1 };
    if number < 0 {
        i64::try_from(number)
            .map(Value::from)
            .map_err(|_| "Number is outside the signed 64-bit range.".into())
    } else {
        u64::try_from(number)
            .map(Value::from)
            .map_err(|_| "Number is outside the unsigned 64-bit range.".into())
    }
}

fn compare_numbers(left: &Value, right: &Value) -> Option<std::cmp::Ordering> {
    match (integer_value(left), integer_value(right)) {
        (Some(left), Some(right)) => Some(left.cmp(&right)),
        _ => left.as_f64()?.partial_cmp(&right.as_f64()?),
    }
}

fn parse(schema: &Value, text: &str) -> Result<Value, String> {
    match schema["type"].as_str() {
        Some("string") => Ok(Value::String(text.to_owned())),
        Some("integer") if schema.get("x-input-units").is_some() => parse_quantity(schema, text),
        Some("integer" | "number" | "boolean") => serde_json::from_str(text)
            .map_err(|_| "Enter a valid JSON number or boolean.".to_owned()),
        Some("array") if schema["items"]["type"] == "string" => {
            if text.trim_start().starts_with('[') {
                serde_json::from_str(text)
                    .map_err(|_| "Enter a valid JSON array of strings.".to_owned())
            } else {
                Ok(Value::Array(
                    text.split(',')
                        .map(|s| Value::String(s.trim().to_owned()))
                        .collect(),
                ))
            }
        }
        Some("array" | "object") => {
            serde_json::from_str(text).map_err(|_| "Enter valid JSON.".to_owned())
        }
        _ => Err("Unsupported schema; use the CLI for this fault.".to_owned()),
    }
}

fn insert(value: &mut Value, path: &[String], item: Value) {
    if let Some((head, tail)) = path.split_first() {
        if !value.is_object() {
            *value = Value::Object(Map::new());
        }
        if let Some(object) = value.as_object_mut() {
            if tail.is_empty() {
                object.insert(head.clone(), item);
            } else {
                insert(
                    object
                        .entry(head.clone())
                        .or_insert_with(|| Value::Object(Map::new())),
                    tail,
                    item,
                );
            }
        }
    } else {
        *value = item;
    }
}

fn apply_constants(schema: &Value, value: &mut Value) {
    if let Some(constant) = schema.get("const") {
        *value = constant.clone();
        return;
    }
    if let Some(properties) = schema["properties"].as_object() {
        for (name, child) in properties {
            let mut item = value.get(name).cloned().unwrap_or(Value::Null);
            apply_constants(child, &mut item);
            if !item.is_null() {
                insert(value, std::slice::from_ref(name), item);
            }
        }
    }
}

/// Deliberately bounded schema support; the server performs final action validation.
pub(super) fn validate(schema: &Value, value: &Value, path: &str) -> Result<(), String> {
    let fail = |message: &str| Err(format!("{path}: {message}"));
    for keyword in ["$ref", "patternProperties"] {
        if schema.get(keyword).is_some() {
            return fail("Unsupported schema; use the CLI for this fault.");
        }
    }
    if let Some(pattern) = schema.get("pattern") {
        let Some(pattern) = pattern.as_str() else {
            return fail("Unsupported schema; use the CLI for this fault.");
        };
        let valid = match pattern {
            "^[a-z0-9][a-z0-9-]{0,62}$" => value.as_str().is_none_or(|text| {
                !text.is_empty()
                    && text.len() <= 63
                    && text
                        .bytes()
                        .next()
                        .is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
                    && text
                        .bytes()
                        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'-')
            }),
            r"^[^=\u0000]+$" => value
                .as_str()
                .is_none_or(|text| !text.is_empty() && !text.contains('=') && !text.contains('\0')),
            r"^[^\u0000]*$" => value.as_str().is_none_or(|text| !text.contains('\0')),
            _ => return fail("Unsupported schema; use the CLI for this fault."),
        };
        if !valid {
            return fail("Text does not match the required format.");
        }
    }
    if schema == &Value::Bool(false) {
        return fail("Value is disallowed by the schema.");
    }
    if let Some(constant) = schema.get("const") {
        if constant != value {
            return fail("Value must match the schema constant.");
        }
    }
    if let Some(children) = schema["allOf"].as_array() {
        for child in children {
            validate(child, value, path)?;
        }
    }
    for (key, exact) in [("oneOf", true), ("anyOf", false)] {
        if let Some(children) = schema[key].as_array() {
            let matches = children
                .iter()
                .filter(|child| validate(child, value, path).is_ok())
                .count();
            if matches == 0 || (exact && matches != 1) {
                return fail("Choose a valid combination of fields; see the fault description.");
            }
        }
    }
    if let Some(child) = schema.get("not") {
        if validate(child, value, path).is_ok() {
            return fail("This combination or value is disallowed.");
        }
    }
    if let Some(condition) = schema.get("if") {
        let branch = if validate(condition, value, path).is_ok() {
            "then"
        } else {
            "else"
        };
        if let Some(child) = schema.get(branch) {
            validate(child, value, path)?;
        }
    }
    if let Some(options) = schema["enum"].as_array() {
        if !options.contains(value) {
            return fail("Choose a currently available value.");
        }
    }
    let kind = schema["type"].as_str().or_else(|| {
        if schema.get("type").is_some() {
            return None;
        }
        if value.is_object() {
            Some("object")
        } else if value.is_array() {
            Some("array")
        } else if value.is_number() {
            Some("number")
        } else if value.is_string() {
            Some("string")
        } else if value.is_boolean() {
            Some("boolean")
        } else {
            Some("null")
        }
    });
    match kind {
        Some("object") if value.is_object() => {
            if let Some(required) = schema["required"].as_array() {
                for name in required.iter().filter_map(Value::as_str) {
                    if value.get(name).is_none() {
                        return Err(format!("{path}.{name}: Required."));
                    }
                }
            }
            if let Some(dependencies) = schema["dependentRequired"].as_object() {
                for (name, required) in dependencies {
                    if value.get(name).is_some() {
                        for dependency in required
                            .as_array()
                            .into_iter()
                            .flatten()
                            .filter_map(Value::as_str)
                        {
                            if value.get(dependency).is_none() {
                                return Err(format!("{path}.{dependency}: Required with {name}."));
                            }
                        }
                    }
                }
            }
            if let Some(properties) = schema["properties"].as_object() {
                for (name, child) in properties {
                    if let Some(item) = value.get(name) {
                        validate(child, item, &format!("{path}.{name}"))?;
                    }
                }
            }
            if let Some(fields) = value.as_object() {
                for (name, item) in fields {
                    if let Some(rule) = schema.get("propertyNames") {
                        validate(
                            rule,
                            &Value::String(name.clone()),
                            &format!("{path}.{name}"),
                        )?;
                    }
                    if schema["properties"].get(name).is_some() {
                        continue;
                    }
                    if schema["additionalProperties"] == false {
                        return Err(format!("{path}.{name}: Unknown field."));
                    }
                    if schema["additionalProperties"].is_object() {
                        validate(
                            &schema["additionalProperties"],
                            item,
                            &format!("{path}.{name}"),
                        )?;
                    }
                }
            }
        }
        Some("array") if value.is_array() => {
            if let Some(items) = value.as_array() {
                if schema["minItems"]
                    .as_u64()
                    .is_some_and(|n| (items.len() as u64) < n)
                {
                    return fail("Too few items.");
                }
                if schema["maxItems"]
                    .as_u64()
                    .is_some_and(|n| (items.len() as u64) > n)
                {
                    return fail("Too many items.");
                }
                for (index, item) in items.iter().enumerate() {
                    if schema["uniqueItems"] == true && items[..index].contains(item) {
                        return fail("Items must be unique.");
                    }
                    let rule = schema["prefixItems"]
                        .as_array()
                        .and_then(|prefix| prefix.get(index))
                        .unwrap_or(&schema["items"]);
                    validate(rule, item, &format!("{path}[{index}]"))?;
                }
            }
        }
        Some("string") if value.is_string() => {
            let len = value.as_str().map_or(0, |s| s.chars().count()) as u64;
            if schema["minLength"].as_u64().is_some_and(|n| len < n) {
                return fail("Text is too short.");
            }
            if schema["maxLength"].as_u64().is_some_and(|n| len > n) {
                return fail("Text is too long.");
            }
        }
        Some("null") if value.is_null() => {}
        Some("boolean") if value.is_boolean() => {}
        Some("integer") if value.is_i64() || value.is_u64() => {}
        Some("number") if value.is_number() => {}
        _ => return fail("Unsupported schema or incorrect value type; use the displayed type."),
    }
    if value.is_number() {
        use std::cmp::Ordering;
        for (key, rejected) in [
            ("minimum", vec![Ordering::Less]),
            ("maximum", vec![Ordering::Greater]),
            ("exclusiveMinimum", vec![Ordering::Less, Ordering::Equal]),
            ("exclusiveMaximum", vec![Ordering::Greater, Ordering::Equal]),
        ] {
            if let Some(bound) = schema.get(key) {
                if compare_numbers(value, bound).is_some_and(|order| rejected.contains(&order)) {
                    return fail(&format!(
                        "Value is outside the allowed range: {key} {} {}.",
                        display_field_value(schema, bound),
                        if schema.get("x-display-unit").is_some() {
                            ""
                        } else {
                            schema["x-unit"].as_str().unwrap_or("")
                        }
                    ));
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn schema() -> Value {
        json!({"type":"object", "required":["action","settings"], "properties":{
            "action":{"const":"future_server_fault"},
            "settings":{"type":"object", "required":["machines","strength"], "properties":{
                "machines":{"type":"array", "minItems":1, "uniqueItems":true, "items":{"type":"string","enum":["alpha","beta"]}},
                "strength":{"type":"integer","minimum":1,"maximum":8,"default":3,"x-unit":"widgets"}
            }}
        }})
    }

    #[test]
    fn future_fault_nested_fields_defaults_and_constraints() {
        let schema = schema();
        let mut fields = Vec::new();
        flatten(&schema, &mut Vec::new(), true, None, &mut fields);
        assert_eq!(fields.len(), 2);
        assert_eq!(fields[0].path, ["settings", "machines"]);
        assert_eq!(fields[1].text, "3");
        let mut value = json!({});
        apply_constants(&schema, &mut value);
        insert(
            &mut value,
            &fields[1].path,
            parse(&fields[1].schema, &fields[1].text).expect("default"),
        );
        insert(
            &mut value,
            &fields[0].path,
            parse(&fields[0].schema, "alpha, beta").expect("array"),
        );
        assert_eq!(
            value,
            json!({"action":"future_server_fault","settings":{"machines":["alpha","beta"],"strength":3}})
        );
        assert!(validate(&schema, &value, "fault").is_ok());
        value["settings"]["strength"] = json!(9);
        assert!(validate(&schema, &value, "fault")
            .expect_err("range")
            .contains("range"));
        value["settings"]
            .as_object_mut()
            .expect("object")
            .remove("strength");
        assert!(validate(&schema, &value, "fault")
            .expect_err("required")
            .contains("Required"));
    }

    #[test]
    fn live_target_removal_rejects_preserved_value() {
        let mut schema = schema();
        let value =
            json!({"action":"future_server_fault","settings":{"machines":["alpha"],"strength":3}});
        assert!(validate(&schema, &value, "fault").is_ok());
        schema["properties"]["settings"]["properties"]["machines"]["items"]["enum"] =
            json!(["beta"]);
        assert!(validate(&schema, &value, "fault")
            .expect_err("removed target")
            .contains("currently available"));
    }

    fn catalogue(schema: Value) -> FaultCatalogue {
        FaultCatalogue {
            simulation: "k7m2p9qx".parse().expect("id"),
            faults: vec![seedbox_api::FaultDescriptor {
                category: seedbox_api::FaultCategory::Machine,
                name: "future_server_fault".into(),
                title: "Future fault".into(),
                description: "Supplied by server".into(),
                schema,
            }],
        }
    }

    #[test]
    fn single_choice_arrows_select_directly_and_enter_advances() {
        let mut form = FaultForm::new(catalogue(
            json!({"type":"object","required":["machine"],"properties":{"action":{"const":"stop"},"machine":{"type":"string","enum":["alpha","beta"]}}}),
        ));
        form.input(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(form.fields[0].text.is_empty());
        form.input(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE));
        assert_eq!(form.fields[0].text, "alpha");
        form.input(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE));
        assert_eq!(form.fields[0].text, "beta");
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(100, 35)).unwrap();
        terminal
            .draw(|frame| form.draw(frame, frame.area(), None))
            .unwrap();
        let text = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(text.contains("beta"));
        assert!(text.contains("[ Submit fault ]"));
        assert_eq!(
            form.input(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            FormOutcome::None
        );
        assert_eq!(form.focus, form.fields.len());
        form.update_catalogue(form.catalogue.clone());
        assert_eq!(form.focus, form.fields.len());
        assert_eq!(form.fields[0].text, "beta");
        assert_eq!(
            form.input(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            FormOutcome::Submit(json!({"action":"stop","machine":"beta"}))
        );
    }

    #[test]
    fn preview_renders_server_effects_and_scrolls_to_the_last_line() {
        use ratatui::{backend::TestBackend, Terminal};
        let mut form = FaultForm::new(catalogue(json!({"type":"object", "properties":{
            "action":{"const":"future_server_fault"}, "machine":{"type":"string","default":"beta"}
        }})));
        form.input(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        let action = form.request_preview(7).unwrap();
        let preview = seedbox_api::FaultPreview {
            simulation: "k7m2p9qx".parse().unwrap(),
            revision: 7,
            simulated_time_ns: 42,
            category: seedbox_api::FaultCategory::Network,
            machines: vec!["beta".into(), "gamma".into()],
            links: vec![seedbox_api::FaultPreviewLink {
                from: "beta".into(),
                to: "gamma".into(),
            }],
            expected_effect: "Server supplied expected effect".into(),
            observations: Vec::new(),
            preconditions: (0..20)
                .map(|n| format!("Condition {n} required before admission"))
                .collect(),
            limitations: vec!["Effects are not guaranteed.".into()],
        };
        form.accept_preview(action.clone(), 7, Ok(preview));
        assert!(form.preview_ready(&action));
        assert_eq!(form.targets(), ["beta", "gamma"]);
        assert!(form.targets_link("beta", "gamma"));
        assert!(!form.targets_link("gamma", "beta"));
        form.focus = form.fields.len();
        let render = |form: &FaultForm, width, height| {
            let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
            terminal
                .draw(|frame| form.draw(frame, frame.area(), None))
                .unwrap();
            terminal
                .backend()
                .buffer()
                .content
                .iter()
                .map(|cell| cell.symbol())
                .collect::<String>()
        };
        let first = render(&form, 100, 40);
        assert!(first.contains("Machine · Future fault"));
        assert!(first.contains("Expected targets: beta, gamma"));
        assert!(first.contains("beta → gamma"));
        assert!(first.contains("Server supplied expected effect"));
        for _ in 0..20 {
            form.input(KeyEvent::new(KeyCode::PageDown, KeyModifiers::NONE));
        }
        let last = render(&form, 100, 40);
        assert!(last.contains("Observed revision 7 at 42 ns"), "{last}");
        form.input(KeyEvent::new(KeyCode::PageUp, KeyModifiers::NONE));
        assert_ne!(
            render(&form, 100, 40),
            last,
            "Scrolling up from the end must respond immediately"
        );
        for (width, height) in [(40, 12), (20, 6)] {
            render(&form, width, height);
        }
    }

    #[test]
    fn context_fills_selected_machine_fresh_handles_and_heal_shortcuts() {
        let schema = json!({"type":"object","required":["machine","handle"],"properties":{
            "action":{"const":"partition"},
            "machine":{"type":"string","enum":["alpha","beta"]},
            "handle":{"type":"string"}}});
        let heal = json!({"type":"object","required":["handle"],"properties":{
            "action":{"const":"heal"},"handle":{"type":"string","enum":["p1"]}}});
        let mut catalogue = catalogue(schema);
        catalogue.faults.push(seedbox_api::FaultDescriptor {
            category: seedbox_api::FaultCategory::Machine,
            name: "heal".into(),
            title: "Heal fault".into(),
            description: String::new(),
            schema: heal,
        });
        let context = FormContext {
            selected: Some("beta".into()),
            handles: vec!["p1".into()],
            active: vec![("p1".into(), "partition alpha┃beta".into())],
        };
        let mut form = FaultForm::new(catalogue.clone()).with_context(context.clone());
        // The live heal opens a review before Enter can submit.
        assert_eq!(
            form.input(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            FormOutcome::None
        );
        assert_eq!(
            form.value().unwrap(),
            json!({"action":"heal","handle":"p1"})
        );
        assert_eq!(form.focus, form.fields.len());
        let mut form = FaultForm::new(catalogue).with_context(context);
        for c in "future".chars() {
            form.input(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE));
        }
        form.input(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        let text = |path: &str| {
            form.fields
                .iter()
                .find(|f| f.path.join(".") == path)
                .unwrap()
                .text
                .clone()
        };
        assert_eq!(text("machine"), "beta");
        assert_eq!(text("handle"), "f1");
        assert!(
            form.targets().is_empty(),
            "Only a server preview may highlight targets"
        );
    }

    #[test]
    fn catalogue_refresh_preserves_focused_field_when_schema_adds_a_field() {
        let mut schema = json!({"type":"object","properties":{"action":{"const":"stop"},"machine":{"type":"string"}}});
        let mut form = FaultForm::new(catalogue(schema.clone()));
        form.input(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(form.fields[form.focus].path, ["machine"]);
        schema["properties"]["duration"] = json!({"type":"integer"});
        form.update_catalogue(catalogue(schema));
        assert_eq!(form.fields[form.focus].path, ["machine"]);
    }

    #[test]
    fn refresh_preserves_edits_and_rejects_removed_target() {
        let mut form = FaultForm::new(catalogue(schema()));
        form.input(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        form.fields[0].text = "alpha".into();
        form.fields[1].text = "7".into();
        let mut updated = schema();
        updated["properties"]["settings"]["properties"]["machines"]["items"]["enum"] =
            json!(["beta"]);
        form.update_catalogue(catalogue(updated));
        assert_eq!(form.fields[0].text, "alpha");
        assert_eq!(form.fields[1].text, "7");
        assert!(form
            .value()
            .expect_err("removed")
            .contains("currently available"));
    }

    #[test]
    fn clearing_optional_default_omits_it() {
        let schema = json!({"type":"object","properties":{"action":{"const":"future"},"amount":{"type":"integer","default":5}}});
        let mut form = FaultForm::new(catalogue(schema));
        form.input(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(form.value().expect("default")["amount"], 5);
        form.fields[0].text.clear();
        assert!(form.value().expect("omitted").get("amount").is_none());
    }

    #[test]
    fn combinations_without_type_and_dependencies_are_supported() {
        let schema = json!({"type":"object","properties":{"fixed":{"type":"integer","not":{"const":0}},"bound":{"type":"integer","minimum":1}},
            "oneOf":[{"required":["fixed"],"not":{"required":["bound"]}},{"required":["bound"],"not":{"required":["fixed"]}}]});
        assert!(validate(&schema, &json!({"fixed":2}), "fault").is_ok());
        assert!(validate(&schema, &json!({"fixed":0}), "fault").is_err());
        assert!(validate(&schema, &json!({"fixed":2,"bound":1}), "fault").is_err());
        assert!(validate(&schema, &json!({}), "fault").is_err());
        let condition = json!({"type":"object","allOf":[{"if":{"properties":{"chance":{"minimum":1}},"required":["chance"]},"then":{"properties":{"delay":{"minimum":1}},"required":["delay"]}}],"dependentRequired":{"queue":["bandwidth"]}});
        assert!(validate(&condition, &json!({"chance":0}), "fault").is_ok());
        assert!(validate(&condition, &json!({"chance":1}), "fault").is_err());
        assert!(validate(&condition, &json!({"chance":1,"delay":1}), "fault").is_ok());
        assert!(validate(&condition, &json!({"queue":1}), "fault").is_err());
        assert!(validate(&condition, &json!({"queue":1,"bandwidth":1}), "fault").is_ok());
    }

    #[test]
    fn unsupported_schema_is_explicit() {
        assert!(validate(&json!({"$ref":"future"}), &json!("x"), "fault")
            .expect_err("unsupported")
            .contains("Unsupported"));
    }

    fn spawn_catalogue() -> FaultCatalogue {
        FaultCatalogue {
            simulation: "k7m2p9qx".parse().expect("id"),
            faults: vec![seedbox_api::FaultDescriptor {
                name: "spawn".into(),
                title: "Spawn machine".into(),
                description: String::new(),
                schema: json!({"type":"object","additionalProperties":false,
                "required":["action","from","machine"],"properties":{
                    "action":{"type":"string","const":"spawn"},
                    "from":{"type":"string","enum":["alpha","beta"]},
                    "machine":{"type":"string","minLength":1},
                    "env":{"type":"object","additionalProperties":{"type":"string"}},
                    "argv":{"type":"array","items":{"type":"string"}}
                }}),
            }],
        }
    }

    fn spawn_context() -> FormContext {
        FormContext {
            machines: vec!["alpha".into(), "alpha-2".into(), "beta".into()],
            selected: Some("alpha".into()),
            ..Default::default()
        }
    }

    #[test]
    fn quick_spawn_is_first_and_names_a_fresh_copy() {
        let mut form = FaultForm::new(spawn_catalogue()).with_context(spawn_context());
        assert_eq!(form.entries()[0], Entry::SpawnCopy("alpha".into()));
        let action = json!({"action":"spawn","from":"alpha","machine":"alpha-3"});
        assert_eq!(form.quick_spawn_action(), Some(action.clone()));
        assert_eq!(
            form.input(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            FormOutcome::Submit(action)
        );
        assert!(!form.editing);
        form.context.selected = None;
        assert_eq!(form.quick_spawn_action(), None);
        assert_eq!(form.entries(), vec![Entry::Fault(0)]);
        form.context.selected = Some("removed".into());
        assert_eq!(form.quick_spawn_action(), None);
        assert_eq!(form.entries(), vec![Entry::Fault(0)]);
    }

    #[test]
    fn quick_spawn_keeps_long_names_within_the_dns_limit() {
        let mut form = FaultForm::new(spawn_catalogue()).with_context(spawn_context());
        let long = format!("{}z", "a".repeat(62));
        form.context.selected = Some(long.clone());
        form.catalogue.faults[0].schema["properties"]["from"]["enum"] = json!([long]);
        let name = form.quick_spawn_action().unwrap()["machine"]
            .as_str()
            .unwrap()
            .to_owned();
        assert_eq!(name.len(), 63);
        assert!(name.ends_with("-2"));
        form.context.machines.push(name);
        assert!(form.quick_spawn_action().unwrap()["machine"]
            .as_str()
            .unwrap()
            .ends_with("-3"));
    }

    #[test]
    fn customized_spawn_accepts_json_environment_and_exact_arguments() {
        let mut form = FaultForm::new(spawn_catalogue()).with_context(spawn_context());
        form.input(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        form.input(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(form.editing);
        let text = |path: &str, form: &FaultForm| -> String {
            form.fields
                .iter()
                .find(|field| field.path == [path])
                .expect("field")
                .text
                .clone()
        };
        assert_eq!(text("from", &form), "alpha");
        assert_eq!(text("machine", &form), "alpha-3");
        form.fields
            .iter_mut()
            .find(|field| field.path == ["env"])
            .unwrap()
            .text = r#"{"MODE":"try,it"}"#.into();
        form.fields
            .iter_mut()
            .find(|field| field.path == ["argv"])
            .unwrap()
            .text = r#"["--key=one,two","plain"]"#.into();
        assert_eq!(
            form.value(),
            Ok(
                json!({"action":"spawn","from":"alpha","machine":"alpha-3","env":{"MODE":"try,it"},"argv":["--key=one,two","plain"]})
            )
        );
        form.fields
            .iter_mut()
            .find(|field| field.path == ["env"])
            .unwrap()
            .text = r#"{"MODE":2}"#.into();
        assert!(form.value().unwrap_err().contains("env.MODE"));
        form.fields
            .iter_mut()
            .find(|field| field.path == ["env"])
            .unwrap()
            .text = "".into();
        form.fields
            .iter_mut()
            .find(|field| field.path == ["argv"])
            .unwrap()
            .text = "[]".into();
        assert_eq!(form.value().unwrap()["argv"], json!([]));
    }

    #[test]
    fn spawn_schema_rejects_bad_names_env_and_argv_in_form() {
        let mut schema = spawn_catalogue().faults.remove(0).schema;
        schema["properties"]["machine"]["pattern"] = json!("^[a-z0-9][a-z0-9-]{0,62}$");
        schema["properties"]["env"]["propertyNames"] = json!({"pattern":r"^[^=\u0000]+$"});
        schema["properties"]["env"]["additionalProperties"]["pattern"] = json!(r"^[^\u0000]*$");
        schema["properties"]["argv"]["minItems"] = json!(1);
        schema["properties"]["argv"]["prefixItems"] =
            json!([{"type":"string", "minLength":1, "pattern":r"^[^\u0000]*$"}]);
        schema["properties"]["argv"]["items"]["pattern"] = json!(r"^[^\u0000]*$");
        let base = json!({"action":"spawn","from":"alpha","machine":"alpha-2"});
        assert!(validate(&schema, &base, "fault").is_ok());
        for (property, bad, path) in [
            ("machine", json!("-bad"), "fault.machine"),
            ("machine", json!("A"), "fault.machine"),
            ("env", json!({"A=B":"yes"}), "fault.env.A=B"),
            ("env", json!({"A":"bad\u{0000}value"}), "fault.env.A"),
            ("argv", json!([]), "fault.argv"),
            ("argv", json!([""]), "fault.argv[0]"),
            ("argv", json!(["bin", "bad\u{0000}value"]), "fault.argv[1]"),
        ] {
            let mut action = base.clone();
            action[property] = bad;
            assert!(validate(&schema, &action, "fault")
                .unwrap_err()
                .contains(path));
        }
    }

    #[test]
    fn dynamic_object_editor_does_not_replace_nested_property_fields() {
        let mut fields = Vec::new();
        flatten(&schema(), &mut Vec::new(), true, None, &mut fields);
        assert_eq!(fields.len(), 2);
        assert_eq!(fields[0].path, ["settings", "machines"]);
        assert!(validate(
            &json!({"type":"object","additionalProperties":false}),
            &json!({"surprise":1}),
            "fault"
        )
        .is_err());
    }
}

#[cfg(test)]
mod quantity_tests {
    use super::*;
    use serde_json::json;

    fn duration() -> Value {
        json!({"type":"integer","minimum":-1_000_000_000,"maximum":10_000_000_000u64,"x-unit":"ns","x-input-units":{"ns":1,"us":1000,"ms":1_000_000,"s":1_000_000_000},"x-input-example":"100ms","x-display-unit":"ms"})
    }
    #[test]
    fn decimal_units_are_exact_and_canonical() {
        let schema = duration();
        for (text, value) in [
            ("100ms", 100_000_000),
            ("1.25s", 1_250_000_000),
            ("-0.5 ms", -500_000),
            (".001us", 1),
            ("100", 100),
            ("+2ms", 2_000_000),
        ] {
            let parsed = parse(&schema, text).unwrap();
            assert_eq!(parsed, json!(value), "{text}");
            validate(&schema, &parsed, "delay").unwrap();
        }
        assert_eq!(
            display_field_value(&schema, &json!(1_250_001)),
            "1.250001ms"
        );
        assert_eq!(display_field_value(&schema, &json!(-500_000)), "-0.5ms");
        assert!(parse(&schema, "0.1ns")
            .unwrap_err()
            .contains("whole number of ns"));
        assert!(parse(&schema, "1fortnight")
            .unwrap_err()
            .contains("Unknown unit"));
        assert!(parse(&schema, "1.2.3ms").is_err());
        assert!(parse(&schema, "999999999999999999999999999999999999999s").is_err());
        assert!(validate(&schema, &parse(&schema, "11s").unwrap(), "delay")
            .unwrap_err()
            .contains("10000ms"));
    }
    #[test]
    fn percentages_and_future_server_units_use_metadata() {
        let probability = json!({"type":"integer","minimum":0,"maximum":1_000_000,"x-unit":"ppm","x-input-units":{"%":10_000,"ppm":1}});
        assert_eq!(parse(&probability, "2.5%").unwrap(), json!(25_000));
        assert!(validate(&probability, &parse(&probability, "101%").unwrap(), "loss").is_err());
        assert!(parse(&probability, "-1%").is_ok());
        assert!(validate(&probability, &parse(&probability, "-1%").unwrap(), "loss").is_err());
        let future = json!({"type":"integer","x-unit":"widgets","x-input-units":{"crate":12}});
        assert_eq!(parse(&future, "1.5crate").unwrap(), json!(18));
        let exact_bound = json!({"type":"integer","maximum":u64::MAX-1});
        assert!(validate(&exact_bound, &json!(u64::MAX), "amount").is_err());
        assert_eq!(
            parse(&duration(), &u64::MAX.to_string()).unwrap(),
            json!(u64::MAX)
        );
    }
    #[test]
    fn rejection_keeps_fields_and_allows_corrected_retry() {
        let mut form = FaultForm::new(FaultCatalogue {
            simulation: "k7m2p9qx".parse().unwrap(),
            faults: vec![seedbox_api::FaultDescriptor {
                category: seedbox_api::FaultCategory::Machine,
                name: "future".into(),
                title: "Future".into(),
                description: String::new(),
                schema: json!({"type":"object","required":["delay"],"properties":{"action":{"const":"future"},"delay":duration()}}),
            }],
        });
        form.input(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        form.fields[0].text = "100ms".into();
        form.set_error("Server rejected delay; adjust it and retry.");
        assert_eq!(form.fields[0].text, "100ms");
        assert!(form.error.as_deref().unwrap().contains("retry"));
        form.input(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
        assert!(form.fields[0].text.is_empty());
        for c in "25ms".chars() {
            form.input(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE));
        }
        form.input(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        assert_eq!(
            form.input(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            FormOutcome::Submit(json!({"action":"future","delay":25_000_000}))
        );
    }
}
