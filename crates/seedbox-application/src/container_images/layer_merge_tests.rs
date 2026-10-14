//! The supported layer subset through export merge, rootfs prefixing and newc
//! packaging, against a golden written from Linux extraction semantics: each
//! layer applied in order over the previous result, as `docker load` does.
use super::prefix_archive;
use crate::{execution::CancellationToken, image_archive::merge};
use std::{collections::BTreeMap, fs, io::Write, path::Path};

/// One layer, built entry by entry in the order an engine would write it.
struct Layer {
    tar: tar::Builder<Vec<u8>>,
    mtime: u64,
}

impl Layer {
    fn new(mtime: u64) -> Self {
        Self {
            tar: tar::Builder::new(Vec::new()),
            mtime,
        }
    }
    fn header(&self, kind: tar::EntryType, mode: u32) -> tar::Header {
        let mut header = tar::Header::new_ustar();
        header.set_entry_type(kind);
        header.set_mode(mode);
        header.set_uid(0);
        header.set_gid(0);
        header.set_mtime(self.mtime);
        header.set_size(0);
        header
    }
    fn dir(&mut self, path: &str, mode: u32) -> &mut Self {
        let mut header = self.header(tar::EntryType::Directory, mode);
        self.tar
            .append_data(&mut header, path, std::io::empty())
            .unwrap();
        self
    }
    fn file(&mut self, path: &str, mode: u32, bytes: &[u8]) -> &mut Self {
        self.owned_file(path, mode, (0, 0), bytes)
    }
    fn owned_file(&mut self, path: &str, mode: u32, owner: (u64, u64), bytes: &[u8]) -> &mut Self {
        let mut header = self.header(tar::EntryType::Regular, mode);
        header.set_uid(owner.0);
        header.set_gid(owner.1);
        header.set_size(bytes.len() as u64);
        self.tar.append_data(&mut header, path, bytes).unwrap();
        self
    }
    fn symlink(&mut self, path: &str, target: &str) -> &mut Self {
        let mut header = self.header(tar::EntryType::Symlink, 0o777);
        self.tar.append_link(&mut header, path, target).unwrap();
        self
    }
    fn hardlink(&mut self, path: &str, target: &str) -> &mut Self {
        let mut header = self.header(tar::EntryType::Link, 0o644);
        self.tar.append_link(&mut header, path, target).unwrap();
        self
    }
    /// `.wh.<name>` beside `name`, or `.wh..wh..opq` for `name = None`.
    fn whiteout(&mut self, directory: &str, name: Option<&str>) -> &mut Self {
        let marker = match name {
            Some(name) => format!("{directory}.wh.{name}"),
            None => format!("{directory}.wh..wh..opq"),
        };
        self.file(&marker, 0o644, b"")
    }
    fn pax(&mut self, records: &[(&str, &[u8])]) -> &mut Self {
        self.tar
            .append_pax_extensions(records.iter().map(|(k, v)| (*k, *v)))
            .unwrap();
        self
    }
    /// An old-GNU (typeflag `S`) sparse file: one stored chunk at `offset`.
    fn gnu_sparse(&mut self, path: &str, offset: u64, chunk: &[u8], real: u64) -> &mut Self {
        fn octal(field: &mut [u8], value: u64) {
            let text = format!("{value:0width$o}\0", width = field.len() - 1);
            field.copy_from_slice(text.as_bytes());
        }
        let mut header = tar::Header::new_gnu();
        header.set_path(path).unwrap();
        header.set_entry_type(tar::EntryType::GNUSparse);
        header.set_mode(0o644);
        header.set_uid(0);
        header.set_gid(0);
        header.set_mtime(self.mtime);
        header.set_size(chunk.len() as u64);
        let gnu = header.as_gnu_mut().unwrap();
        octal(&mut gnu.sparse[0].offset, offset);
        octal(&mut gnu.sparse[0].numbytes, chunk.len() as u64);
        // GNU tar closes the map with an empty chunk at the real size.
        octal(&mut gnu.sparse[1].offset, real);
        octal(&mut gnu.sparse[1].numbytes, 0);
        octal(&mut gnu.realsize, real);
        header.set_cksum();
        self.tar.append(&header, chunk).unwrap();
        self
    }
    /// A PAX format 1.0 sparse file: the map leads the stored data and the
    /// header names a `GNUSparseFile.0` placeholder.
    fn pax_sparse(&mut self, path: &str, offset: u64, chunk: &[u8], real: u64) -> &mut Self {
        let (directory, name) = path.rsplit_once('/').unwrap();
        let real_text = real.to_string();
        self.pax(&[
            ("GNU.sparse.major", b"1"),
            ("GNU.sparse.minor", b"0"),
            ("GNU.sparse.name", path.as_bytes()),
            ("GNU.sparse.realsize", real_text.as_bytes()),
        ]);
        let mut data = format!("1\n{offset}\n{}\n", chunk.len()).into_bytes();
        data.resize(512, 0);
        data.extend_from_slice(chunk);
        let mut header = self.header(tar::EntryType::Regular, 0o644);
        header.set_size(data.len() as u64);
        self.tar
            .append_data(
                &mut header,
                format!("{directory}/GNUSparseFile.0/{name}"),
                data.as_slice(),
            )
            .unwrap();
        self
    }
    fn finish(self) -> Vec<u8> {
        self.tar.into_inner().unwrap()
    }
}

/// A Docker-save envelope holding `layers`, oldest first.
fn envelope(path: &Path, layers: Vec<Vec<u8>>) {
    let names: Vec<_> = (0..layers.len())
        .map(|i| format!("blobs/sha256/layer{i}"))
        .collect();
    let manifest = serde_json::json!([{"Config": "config.json", "Layers": names}]).to_string();
    let mut image = tar::Builder::new(fs::File::create(path).unwrap());
    let mut members = vec![
        ("manifest.json".to_owned(), manifest.into_bytes()),
        (
            "config.json".to_owned(),
            br#"{"architecture":"arm64"}"#.to_vec(),
        ),
    ];
    members.extend(names.into_iter().zip(layers));
    for (name, bytes) in members {
        let mut header = tar::Header::new_gnu();
        header.set_mode(0o644);
        header.set_size(bytes.len() as u64);
        image
            .append_data(&mut header, name, bytes.as_slice())
            .unwrap();
    }
    image.finish().unwrap();
}

/// Run the preparation pipeline: merge, prefix, package. Returns the newc
/// bytes, or the first error.
fn package(root: &Path, layers: Vec<Vec<u8>>) -> crate::Result<Vec<u8>> {
    let image = root.join("image.tar");
    envelope(&image, layers);
    merge(
        &image,
        &root.join("layers"),
        &root.join("filesystem.tar"),
        &CancellationToken::default(),
    )?;
    prefix_archive(
        &root.join("filesystem.tar"),
        &root.join("rootfs.tar"),
        "example/image",
        &|| Ok(()),
    )?;
    fs::write(root.join("init"), b"INIT").unwrap();
    crate::guest::build_initramfs_from_tar(
        &root.join("rootfs.tar"),
        &root.join("init"),
        None,
        None,
        b"RECIPE",
        &root.join("initramfs"),
    )?;
    Ok(fs::read(root.join("initramfs")).unwrap())
}

struct Newc {
    name: String,
    inode: u32,
    mode: u32,
    uid: u32,
    gid: u32,
    nlink: u32,
    mtime: u32,
    rdev: (u32, u32),
    data: Vec<u8>,
}

/// An independent newc reader: 110-byte ASCII headers, names and bodies
/// padded to four bytes, ending at `TRAILER!!!`.
fn parse(bytes: &[u8]) -> Vec<Newc> {
    let pad = |n: usize| n.next_multiple_of(4);
    let mut at = 0;
    let mut entries = Vec::new();
    loop {
        let header = std::str::from_utf8(&bytes[at..at + 110]).unwrap();
        assert_eq!(&header[..6], "070701", "newc magic at {at}");
        let field = |i: usize| u32::from_str_radix(&header[6 + 8 * i..14 + 8 * i], 16).unwrap();
        let (size, name_size) = (field(6) as usize, field(11) as usize);
        let name = &bytes[at + 110..at + 110 + name_size];
        assert_eq!(name.last(), Some(&0));
        let name = String::from_utf8(name[..name_size - 1].to_vec()).unwrap();
        let body = pad(at + 110 + name_size);
        at = pad(body + size);
        if name == "TRAILER!!!" {
            assert_eq!(at, bytes.len(), "trailer ends the archive");
            return entries;
        }
        entries.push(Newc {
            name,
            inode: field(0),
            mode: field(1),
            uid: field(2),
            gid: field(3),
            nlink: field(4),
            mtime: field(5),
            rdev: (field(9), field(10)),
            data: bytes[body..body + size].to_vec(),
        });
    }
}

/// Text bodies verbatim; anything else as its length and nonzero runs.
fn body(data: &[u8]) -> String {
    match std::str::from_utf8(data) {
        Ok(text) if !data.contains(&0) => format!("{text:?}"),
        _ => {
            let mut runs = Vec::new();
            let mut i = 0;
            while i < data.len() {
                if data[i] == 0 {
                    i += 1;
                    continue;
                }
                let start = i;
                while i < data.len() && data[i] != 0 {
                    i += 1;
                }
                runs.push(format!(
                    "{start}:{:?}",
                    String::from_utf8_lossy(&data[start..i])
                ));
            }
            format!("{} bytes, nonzero {}", data.len(), runs.join(" "))
        }
    }
}

/// The archive as sorted listing lines. Hard links name the first path of
/// their inode instead of a number; the kernel takes the data from the first
/// entry it sees for an inode, so later ones must carry none.
fn listing(entries: &[Newc]) -> Vec<String> {
    let mut first = BTreeMap::new();
    let mut lines = Vec::new();
    for entry in entries {
        let shared = entry.nlink > 1 && entry.mode & 0o170000 == 0o100000;
        let content = match first.get(&entry.inode) {
            Some(name) if shared => {
                assert!(entry.data.is_empty(), "{} repeats inode data", entry.name);
                format!("link to {name}")
            }
            _ => {
                if shared {
                    first.insert(entry.inode, entry.name.clone());
                }
                body(&entry.data)
            }
        };
        lines.push(format!(
            "{} {:06o} {}:{} n{} t{} {}",
            entry.name, entry.mode, entry.uid, entry.gid, entry.nlink, entry.mtime, content
        ));
    }
    lines.sort();
    lines
}

/// The kernel's initramfs unpacker creates no missing parents and skips an
/// entry it cannot create without an error, so each parent must come first.
fn parents_precede_children(entries: &[Newc]) {
    let mut seen = std::collections::BTreeSet::new();
    for entry in entries {
        if let Some((parent, _)) = entry.name.rsplit_once('/') {
            assert!(
                seen.contains(parent),
                "{} precedes its parent directory",
                entry.name
            );
        }
        if entry.mode & 0o170000 == 0o040000 {
            seen.insert(entry.name.clone());
        }
    }
}

fn layers() -> Vec<Vec<u8>> {
    let mut base = Layer::new(1000);
    base.dir("./", 0o755)
        .dir("bin/", 0o755)
        .file("bin/tool", 0o4755, b"tool-v0")
        .file("bin/sh", 0o755, b"sh")
        .dir("tmp/", 0o1777)
        .dir("etc/", 0o755)
        .pax(&[("uid", b"3000000"), ("gid", b"3000001")])
        .file("etc/keep", 0o640, b"keep")
        .file("etc/gone", 0o644, b"gone")
        .file("etc/hosts", 0o644, b"h0")
        .dir("opq/", 0o755)
        .file("opq/old", 0o644, b"o1")
        .dir("opq/sub/", 0o755)
        .file("opq/sub/old", 0o644, b"o2")
        .file("swap", 0o644, b"file-v0")
        .dir("tree/", 0o755)
        .file("tree/leaf", 0o644, b"leaf")
        .dir("data/", 0o755)
        .file("data/orig", 0o644, b"orig-content")
        .hardlink("data/alias", "data/orig")
        .hardlink("data/third", "./data/orig")
        .dir("links/", 0o755)
        .file("links/target", 0o644, b"T")
        .symlink("links/l3", "target")
        .symlink("links/l2", "l3")
        .symlink("links/l1", "l2")
        .symlink("links/abs", "/etc/hosts")
        .symlink("links/dangling", "missing")
        .dir("unicode/", 0o755)
        .owned_file("unicode/naïve-ü.txt", 0o600, (1000, 1000), b"u")
        .file("unicode/日本語", 0o644, b"jp")
        .file("empty", 0o644, b"")
        .dir("wd/", 0o755)
        .file("wd/f", 0o644, b"w0");
    let mut middle = Layer::new(1001);
    middle
        .dir("etc/", 0o755)
        .whiteout("etc/", Some("gone"))
        .file("etc/hosts", 0o644, b"h1")
        .dir("opq/", 0o750)
        .whiteout("opq/", None)
        .file("opq/new", 0o644, b"n1")
        // File to directory: the whiteout and the new directory share a layer.
        .whiteout("", Some("swap"))
        .dir("swap/", 0o755)
        .file("swap/inner", 0o644, b"in")
        // Replacing a hard-link target leaves the other links on the old inode.
        .dir("data/", 0o755)
        .file("data/orig", 0o644, b"replaced")
        .dir("bin/", 0o755)
        .whiteout("bin/", Some("sh"))
        // An opaque directory recreated with a file in the same layer.
        .dir("wd/", 0o755)
        .whiteout("wd/", None)
        .file("wd/f", 0o644, b"w1")
        .dir("sparse/", 0o755)
        .gnu_sparse("sparse/old-gnu", 65536, b"gnu", 131072)
        .pax_sparse("sparse/pax", 4096, b"hello", 8192)
        .dir("hl/", 0o755)
        .file("hl/a", 0o644, b"A")
        .hardlink("hl/b", "hl/a")
        .hardlink("hl/c", "hl/a");
    let mut top = Layer::new(1002);
    top
        // Directory back to a file.
        .whiteout("", Some("swap"))
        .file("swap", 0o644, b"file-v2")
        // A symlink covers an older directory without a whiteout.
        .symlink("tree", "/etc")
        // A whiteout below a directory an older layer made opaque.
        .dir("wd/", 0o755)
        .whiteout("wd/", Some("f"))
        // Whiting out a link target promotes its surviving links.
        .dir("hl/", 0o755)
        .whiteout("hl/", Some("a"));
    vec![base.finish(), middle.finish(), top.finish()]
}

/// Written from applying the three layers in order, not from Seedbox output.
const GOLDEN: &[&str] = &[
    r#"init 100755 0:0 n1 t0 "INIT""#,
    "rootfs 040755 0:0 n2 t1000 \"\"",
    "rootfs/bin 040755 0:0 n2 t1001 \"\"",
    r#"rootfs/bin/tool 104755 0:0 n1 t1000 "tool-v0""#,
    "rootfs/data 040755 0:0 n2 t1001 \"\"",
    r#"rootfs/data/alias 100644 0:0 n2 t1000 "orig-content""#,
    r#"rootfs/data/orig 100644 0:0 n1 t1001 "replaced""#,
    "rootfs/data/third 100644 0:0 n2 t1000 link to rootfs/data/alias",
    "rootfs/empty 100644 0:0 n1 t1000 \"\"",
    "rootfs/etc 040755 0:0 n2 t1001 \"\"",
    r#"rootfs/etc/hosts 100644 0:0 n1 t1001 "h1""#,
    r#"rootfs/etc/keep 100640 3000000:3000001 n1 t1000 "keep""#,
    "rootfs/hl 040755 0:0 n2 t1002 \"\"",
    r#"rootfs/hl/b 100644 0:0 n2 t1001 "A""#,
    "rootfs/hl/c 100644 0:0 n2 t1001 link to rootfs/hl/b",
    "rootfs/links 040755 0:0 n2 t1000 \"\"",
    r#"rootfs/links/abs 120777 0:0 n1 t1000 "/etc/hosts""#,
    r#"rootfs/links/dangling 120777 0:0 n1 t1000 "missing""#,
    r#"rootfs/links/l1 120777 0:0 n1 t1000 "l2""#,
    r#"rootfs/links/l2 120777 0:0 n1 t1000 "l3""#,
    r#"rootfs/links/l3 120777 0:0 n1 t1000 "target""#,
    r#"rootfs/links/target 100644 0:0 n1 t1000 "T""#,
    "rootfs/opq 040750 0:0 n2 t1001 \"\"",
    r#"rootfs/opq/new 100644 0:0 n1 t1001 "n1""#,
    "rootfs/sparse 040755 0:0 n2 t1001 \"\"",
    r#"rootfs/sparse/old-gnu 100644 0:0 n1 t1001 131072 bytes, nonzero 65536:"gnu""#,
    r#"rootfs/sparse/pax 100644 0:0 n1 t1001 8192 bytes, nonzero 4096:"hello""#,
    r#"rootfs/swap 100644 0:0 n1 t1002 "file-v2""#,
    "rootfs/tmp 041777 0:0 n2 t1000 \"\"",
    r#"rootfs/tree 120777 0:0 n1 t1002 "/etc""#,
    "rootfs/unicode 040755 0:0 n2 t1000 \"\"",
    r#"rootfs/unicode/naïve-ü.txt 100600 1000:1000 n1 t1000 "u""#,
    r#"rootfs/unicode/日本語 100644 0:0 n1 t1000 "jp""#,
    "rootfs/wd 040755 0:0 n2 t1002 \"\"",
    "seedbox 040755 0:0 n1 t0 \"\"",
    r#"seedbox/launch.bin 100400 0:0 n1 t0 "RECIPE""#,
];

#[test]
fn merged_layers_package_the_extracted_tree() {
    let root = tempfile::tempdir().unwrap();
    let entries = parse(&package(root.path(), layers()).unwrap());
    parents_precede_children(&entries);
    let mut golden: Vec<_> = GOLDEN.iter().map(|line| line.to_string()).collect();
    golden.sort();
    let actual = listing(&entries);
    assert!(
        actual == golden,
        "newc listing differs from the golden\nactual:\n{}\nexpected:\n{}",
        actual.join("\n"),
        golden.join("\n")
    );
}

/// A base layer holding only the root, then one layer with `entry`.
fn with_entry(build: impl FnOnce(&mut Layer)) -> crate::Result<Vec<u8>> {
    let mut base = Layer::new(1);
    base.dir("./", 0o755).dir("bin/", 0o755);
    let mut layer = Layer::new(2);
    layer.dir("bin/", 0o755);
    build(&mut layer);
    package(
        tempfile::tempdir().unwrap().path(),
        vec![base.finish(), layer.finish()],
    )
}

#[test]
fn extended_metadata_outside_overlay_bookkeeping_is_rejected() {
    for key in [
        "SCHILY.xattr.security.capability",
        "SCHILY.xattr.security.selinux",
        "SCHILY.xattr.system.posix_acl_access",
        "SCHILY.xattr.user.mime_type",
        "SCHILY.xattr.trusted.other",
        "LIBARCHIVE.xattr.security.capability",
    ] {
        let failure = with_entry(|layer| {
            layer
                .pax(&[(key, &[1, 0, 0, 2, 0x20, 0, 0, 0][..])])
                .file("bin/ping", 0o755, b"ping");
        })
        .err()
        .unwrap_or_else(|| panic!("{key} was packaged"))
        .to_string();
        assert!(
            failure.contains("bin/ping") && failure.contains(key),
            "{key}: {failure}"
        );
    }
    for key in [
        "SCHILY.xattr.trusted.overlay.opaque",
        "SCHILY.xattr.user.overlay.opaque",
        "SCHILY.xattr.trusted.overlay.origin",
        "SCHILY.xattr.user.overlay.redirect",
    ] {
        let entries = parse(
            &with_entry(|layer| {
                layer.pax(&[(key, b"y")]).file("bin/app", 0o755, b"app");
            })
            .unwrap_or_else(|error| panic!("{key}: {error}")),
        );
        assert!(entries.iter().any(|e| e.name == "rootfs/bin/app"), "{key}");
    }
}

impl Layer {
    fn device(
        &mut self,
        path: &str,
        kind: tar::EntryType,
        (major, minor): (u32, u32),
    ) -> &mut Self {
        let mut header = self.header(kind, 0o620);
        header.set_device_major(major).unwrap();
        header.set_device_minor(minor).unwrap();
        self.tar
            .append_data(&mut header, path, std::io::empty())
            .unwrap();
        self
    }
}

/// Official MySQL images keep their base layer's device nodes and the
/// `dev/initctl` FIFO. The guest mounts its own `/dev` over the image's, so
/// these stay inert; the same kinds elsewhere keep their type and number.
#[test]
fn device_nodes_and_fifos_keep_their_type_and_number() {
    use tar::EntryType::{Block, Char, Fifo};
    let entries = parse(
        &with_entry(|layer| {
            layer
                .dir("dev/", 0o755)
                .device("dev/null", Char, (1, 3))
                .device("dev/vda", Block, (254, 0))
                .device("dev/initctl", Fifo, (0, 0))
                .hardlink("dev/initctl-alias", "dev/initctl")
                .device("bin/null", Char, (1, 3))
                .device("bin/disk", Block, (8, 1))
                .device("bin/pipe", Fifo, (0, 0))
                .hardlink("bin/pipe-alias", "bin/pipe");
        })
        .unwrap(),
    );
    let get = |name: &str| {
        entries
            .iter()
            .find(|entry| entry.name == name)
            .unwrap_or_else(|| panic!("{name} missing"))
    };
    for (name, mode, rdev, nlink) in [
        ("rootfs/dev/null", 0o020620, (1, 3), 1),
        ("rootfs/dev/vda", 0o060620, (254, 0), 1),
        ("rootfs/dev/initctl", 0o010620, (0, 0), 2),
        ("rootfs/dev/initctl-alias", 0o010644, (0, 0), 2),
        ("rootfs/bin/null", 0o020620, (1, 3), 1),
        ("rootfs/bin/disk", 0o060620, (8, 1), 1),
        ("rootfs/bin/pipe", 0o010620, (0, 0), 2),
        ("rootfs/bin/pipe-alias", 0o010644, (0, 0), 2),
    ] {
        let entry = get(name);
        assert_eq!(
            (entry.mode, entry.rdev, entry.nlink, entry.data.len()),
            (mode, rdev, nlink, 0),
            "{name}"
        );
    }
    // The kernel links an entry to an earlier one with the same inode and type.
    assert_eq!(
        get("rootfs/dev/initctl").inode,
        get("rootfs/dev/initctl-alias").inode
    );
    assert_eq!(
        get("rootfs/bin/pipe").inode,
        get("rootfs/bin/pipe-alias").inode
    );
    assert_ne!(
        get("rootfs/dev/initctl").inode,
        get("rootfs/bin/pipe").inode
    );
    // Ordinary entries keep a zero device number.
    assert_eq!(get("rootfs/bin").rdev, (0, 0));
}

/// What newc cannot represent fails before packaging, naming the image, the
/// entry and its kind.
#[test]
fn unsupported_entries_name_the_image_and_path() {
    let root = tempfile::tempdir().unwrap();
    let input = root.path().join("filesystem.tar");
    let mut tar = tar::Builder::new(fs::File::create(&input).unwrap());
    let mut header = tar::Header::new_ustar();
    header.set_entry_type(tar::EntryType::Continuous);
    header.set_mode(0o644);
    header.set_size(1);
    tar.append_data(&mut header, "opt/contiguous", &b"x"[..])
        .unwrap();
    tar.finish().unwrap();
    let failure = prefix_archive(
        &input,
        &root.path().join("rootfs.tar"),
        "example/image@sha256:abc",
        &|| Ok(()),
    )
    .unwrap_err();
    assert!(matches!(failure, crate::Error::Setup(_)), "{failure:?}");
    assert_eq!(
        failure.to_string(),
        "image example/image@sha256:abc: /opt/contiguous is a contiguous file, which a \
         Seedbox root filesystem cannot hold; remove it from the image, for example \
         with RUN rm in a derived Dockerfile"
    );
}

/// newc sizes are 32 bits. A sparse host file reaches the limit without
/// writing 4 GiB.
#[test]
fn files_beyond_the_newc_size_field_are_rejected() {
    let root = tempfile::tempdir().unwrap();
    let tree = root.path().join("tree");
    fs::create_dir(&tree).unwrap();
    fs::File::create(tree.join("big"))
        .unwrap()
        .set_len(u64::from(u32::MAX) + 1)
        .unwrap();
    fs::write(root.path().join("init"), b"INIT").unwrap();
    let failure = crate::guest::build_initramfs(
        &tree,
        &root.path().join("init"),
        b"RECIPE",
        &root.path().join("initramfs"),
    )
    .unwrap_err()
    .to_string();
    assert!(failure.contains("exceeds newc 4 GiB limit"), "{failure}");
}

/// Envelope members are bounded before any byte is copied out: one member
/// declaring more than 4 GiB, and metadata over its own 4 MiB bound.
#[test]
fn envelope_size_limits_hold() {
    let root = tempfile::tempdir().unwrap();
    let image = root.path().join("image.tar");
    let mut file = fs::File::create(&image).unwrap();
    let mut header = tar::Header::new_gnu();
    header.set_path("blobs/sha256/huge").unwrap();
    header.set_mode(0o644);
    header.set_size((4 << 30) + 1);
    header.set_cksum();
    file.write_all(header.as_bytes()).unwrap();
    // The body is a hole: nothing is written or read before the check.
    file.set_len(512 + (4 << 30) + 512 + 1024).unwrap();
    drop(file);
    let failure = merge(
        &image,
        &root.path().join("layers"),
        &root.path().join("filesystem.tar"),
        &CancellationToken::default(),
    )
    .unwrap_err()
    .to_string();
    assert!(failure.contains("image archive exceeds 4 GiB"), "{failure}");
    assert!(!root.path().join("layers/blobs/sha256/huge").exists());

    // JSON padded with whitespace past the bound is otherwise valid.
    let manifest = br#"[{"Config":"config.json","Layers":[]}]"#.as_slice();
    let padded = |json: &[u8]| {
        let mut bytes = json.to_vec();
        bytes.resize((4 << 20) + 1, b' ');
        bytes
    };
    for (member, manifest, config) in [
        ("manifest.json", padded(manifest), b"{}".to_vec()),
        ("config.json", manifest.to_vec(), padded(b"{}")),
    ] {
        let root = tempfile::tempdir().unwrap();
        let image = root.path().join("image.tar");
        let mut tar = tar::Builder::new(fs::File::create(&image).unwrap());
        for (name, bytes) in [("manifest.json", manifest), ("config.json", config)] {
            let mut header = tar::Header::new_gnu();
            header.set_mode(0o644);
            header.set_size(bytes.len() as u64);
            tar.append_data(&mut header, name, bytes.as_slice())
                .unwrap();
        }
        tar.finish().unwrap();
        drop(tar);
        let failure = merge(
            &image,
            &root.path().join("layers"),
            &root.path().join("filesystem.tar"),
            &CancellationToken::default(),
        )
        .unwrap_err()
        .to_string();
        assert!(
            failure.contains("image metadata") && failure.contains("exceeds"),
            "{member}: {failure}"
        );
    }
}
