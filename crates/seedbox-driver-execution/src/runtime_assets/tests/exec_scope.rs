//! Runtime extraction writes the QEMU launcher/loader/binary that another
//! process of the execution later executes. Pause a member mid-body (its
//! destination provably open) and check which members exclude forks.
use super::*;
use std::{
    io::Cursor,
    sync::mpsc::{channel, Receiver, Sender},
    thread,
};

struct Pausing {
    data: Cursor<Vec<u8>>,
    pause_at: u64,
    paused: Sender<()>,
    release: Receiver<()>,
    done: bool,
}
impl Read for Pausing {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        let position = self.data.position();
        if position >= self.pause_at && !self.done {
            self.done = true;
            self.paused.send(()).unwrap();
            self.release.recv().unwrap();
        }
        let limit = if self.done {
            buffer.len()
        } else {
            buffer.len().min((self.pause_at - position) as usize)
        };
        self.data.read(&mut buffer[..limit])
    }
}

fn extract_paused(name: &str, mode: u32) -> (bool, Vec<u8>, u32) {
    // `excluded` is only meaningful as a positive: other unit tests may also
    // hold the process-wide scope, which can only add exclusion.
    let body = vec![7u8; 4096];
    let mut builder = tar::Builder::new(vec![]);
    let mut header = tar::Header::new_gnu();
    header.set_size(body.len() as u64);
    header.set_mode(mode);
    builder.append_data(&mut header, name, &body[..]).unwrap();
    let archive = builder.into_inner().unwrap();
    let (paused, wait) = channel();
    let (release, hold) = channel();
    let reader = Pausing {
        data: Cursor::new(archive),
        pause_at: 512 + 1024, // header, then part of the body
        paused,
        release: hold,
        done: false,
    };
    let root = tempfile::tempdir().unwrap();
    let path = root.path().to_owned();
    let extraction = thread::spawn(move || {
        archive::extract(&mut tar::Archive::new(reader), &path).unwrap();
        path
    });
    wait.recv().unwrap();
    let excluded = crate::exec_files::try_spawning().is_none();
    release.send(()).unwrap();
    let path = extraction.join().unwrap();
    let file = path.join(name);
    (
        excluded,
        fs::read(&file).unwrap(),
        fs::metadata(&file).unwrap().permissions().mode() & 0o777,
    )
}

#[test]
fn executable_members_exclude_forks_while_their_destination_is_open() {
    let launcher = format!("host/{}/bin/qemu-system-x86_64", host_system());
    let (excluded, bytes, mode) = extract_paused(&launcher, 0o755);
    assert!(excluded, "a fork could copy the open launcher descriptor");
    assert_eq!((bytes, mode), (vec![7; 4096], 0o755));
}
