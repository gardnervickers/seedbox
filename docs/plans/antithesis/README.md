# Antithesis-informed roadmap

A plan for what Seedbox should build next, based on Antithesis's public
documentation and Seedbox at commit `ec80ea9f`. Nothing here has been built or
tested yet; every mechanism is a design to validate.

Start with [plan-consensus.md](plan-consensus.md).

| File | Contents |
|---|---|
| [plan-consensus.md](plan-consensus.md) | The agreed plan: capability gaps, phased roadmap with exit criteria, architecture, determinism risks, open decisions |
| [decision-log.md](decision-log.md) | Each disputed point: initial positions, arguments, concessions, resolution |
| [brief.md](brief.md) | The brief both planners worked from |
| [plan-codex.md](plan-codex.md), [plan-opus.md](plan-opus.md) | The two independent plans that preceded consensus |
| [negotiation/](negotiation/) | The three consensus rounds. Opus messages omit their appendices, which repeated the full plans |
| [antithesis-docs-index.md](antithesis-docs-index.md) | Every Antithesis doc page consulted, with URLs |

## How it was produced

On 2026-10-16, Codex (`gpt-6-astra`) and Claude Opus 5.5 each wrote a plan
independently from the brief, with read-only access to the repository and a
local snapshot of the Antithesis docs. Opus then negotiated with Codex for
three rounds until Codex signed off on the exact text of the consensus plan.

## Antithesis docs snapshot

The docs snapshot is not committed because it is Antithesis's content. Plans
cite pages as `pages/<path>.md`, where `<path>` is the URL path after
`/docs/` with `/` replaced by `__`. To recreate the snapshot:

```sh
mkdir -p pages
curl -sL https://antithesis.com/docs/llms.txt \
  | grep -oE 'https://antithesis\.com/docs/[^)]+\.md' | sort -u \
  | while read -r url; do
      curl -sfL -o "pages/$(echo "${url#https://antithesis.com/docs/}" | sed 's#/#__#g')" "$url"
    done
```

The per-language SDK reference pages returned 404 when fetched; the fallback
assertion schema and SDK overview pages cover the same concepts.
