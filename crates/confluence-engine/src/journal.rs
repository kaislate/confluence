//! Crash-safe persistence of mutating commands.
//!
//! The journal is a file of framed `Envelope<Command>`s. On open it is replayed;
//! a torn final frame (crash mid-write) is cut off. `compact` rewrites it
//! atomically (temp file + rename) as the minimal commands for the current state.
//! A journal has exactly one writer: `open` takes an exclusive lock file first
//! and holds it until the `Journal` is dropped.

use std::fs::{self, File, OpenOptions};
use std::io::{self, BufReader, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use confluence_api::{read_envelope_since, write_frame, Command, Envelope, FrameError};

/// The journal's own record format version. It changes only when a journaled
/// command's encoding changes, not with every Control API bump, so an older
/// engine can still read the journal after an upgrade is rolled back.
pub const JOURNAL_VERSION: u16 = 1;

fn record(id: u32, cmd: &Command) -> Envelope<Command> {
    Envelope { version: JOURNAL_VERSION, id, body: cmd.clone() }
}

/// Drops every `SetParam` that a later `SetParam` for the same bus and
/// parameter overrides, before replay: a long drag leaves hundreds of records,
/// and replayed values queue for the plugin's audio side, which does not run
/// yet. Edits are not moved across a (re)load or a state load of their bus, nor
/// across buses being added or removed.
pub fn collapse_params(cmds: Vec<Command>) -> Vec<Command> {
    use std::collections::HashSet;
    let mut seen: HashSet<(confluence_api::BusRef, u32)> = HashSet::new();
    let mut keep = vec![true; cmds.len()];
    for (i, c) in cmds.iter().enumerate().rev() {
        match c {
            // A later edit of the same parameter was already seen: this one is overridden.
            Command::SetParam { bus, param, .. } if !seen.insert((*bus, *param)) => keep[i] = false,
            Command::LoadPlugin { bus, .. } | Command::UnloadPlugin { bus } | Command::SetPluginState { bus, .. } => {
                seen.retain(|(b, _)| b != bus);
            }
            Command::AddBus { .. } | Command::RemoveSlot { .. } => seen.clear(),
            _ => {}
        }
    }
    cmds.into_iter().zip(keep).filter_map(|(c, k)| k.then_some(c)).collect()
}

pub struct Journal {
    path: PathBuf,
    file: File,
    next_id: u32,
    /// Held open without sharing for the journal's lifetime (single writer).
    _lock: File,
}

/// Windows ERROR_SHARING_VIOLATION: another process holds the lock file.
const ERROR_SHARING_VIOLATION: i32 = 32;

fn lock(path: &Path) -> io::Result<File> {
    let lock_path = path.with_extension("lock");
    let mut opts = OpenOptions::new();
    opts.read(true).write(true).create(true).truncate(false);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        opts.share_mode(0);
    }
    opts.open(&lock_path).map_err(|e| {
        if e.raw_os_error() == Some(ERROR_SHARING_VIOLATION) {
            io::Error::new(io::ErrorKind::WouldBlock, format!("journal {} is in use by another engine", path.display()))
        } else {
            e
        }
    })
}

impl Journal {
    /// Opens (creating if needed) the journal and returns it with the commands
    /// to replay, oldest first.
    pub fn open(path: &Path) -> io::Result<(Journal, Vec<Command>)> {
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir)?;
        }
        let lock = lock(path)?;
        let mut file = OpenOptions::new().read(true).write(true).create(true).truncate(false).open(path)?;
        let mut commands = Vec::new();
        let mut good_len = 0u64;
        {
            let mut reader = BufReader::new(&mut file);
            loop {
                // Records from older engines, and from engines that tagged them
                // with their API version, are all valid commands.
                match read_envelope_since::<_, Command>(&mut reader, 1) {
                    Ok(Some(env)) => {
                        commands.push(env.body);
                        good_len = reader.stream_position()?;
                    }
                    Ok(None) => break,
                    Err(FrameError::Io(e)) if e.kind() != io::ErrorKind::UnexpectedEof => return Err(e),
                    // Torn or corrupt tail: keep everything before it.
                    Err(_) => break,
                }
            }
        }
        file.set_len(good_len)?;
        file.seek(SeekFrom::End(0))?;
        let next_id = commands.len() as u32;
        Ok((Journal { path: path.to_path_buf(), file, next_id, _lock: lock }, commands))
    }

    /// Appends a mutating command and flushes it to the OS.
    pub fn append(&mut self, cmd: &Command) -> io::Result<()> {
        write_frame(&mut self.file, &record(self.next_id, cmd)).map_err(io::Error::other)?;
        self.next_id = self.next_id.wrapping_add(1);
        Ok(())
    }

    /// Atomically replaces the journal with `state` (e.g. one SetPoint per live point).
    pub fn compact(&mut self, state: &[Command]) -> io::Result<()> {
        let tmp = self.path.with_extension("tmp");
        {
            let mut out = File::create(&tmp)?;
            for (i, cmd) in state.iter().enumerate() {
                write_frame(&mut out, &record(i as u32, cmd)).map_err(io::Error::other)?;
            }
            out.flush()?;
            out.sync_all()?;
        }
        fs::rename(&tmp, &self.path)?;
        self.file = OpenOptions::new().read(true).append(true).open(&self.path)?;
        self.next_id = state.len() as u32;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set(i: u32) -> Command {
        Command::SetPoint { input: i, output: i, gain_db: -3.0, mute: false, invert: false }
    }

    /// Existing users' journals were written with protocol version 1: they
    /// must replay after the bump, not be judged corrupt and truncated.
    #[test]
    fn runs_of_parameter_edits_collapse_to_their_last_value() {
        use confluence_api::BusRef;
        let p = |at, param, value| Command::SetParam { bus: BusRef::At(at), param, value };
        let load = |at| Command::LoadPlugin { bus: BusRef::At(at), path: "x.clap".into(), plugin_id: "x".into() };
        let cmds = vec![
            load(8),
            p(8, 1, -1.0),
            p(8, 2, 0.5),
            p(8, 1, -2.0),
            p(4, 1, -7.0), // another bus: its own run
            p(8, 1, -3.0),
            load(8), // a reload: edits before it stay before it
            p(8, 1, -4.0),
            p(8, 1, -5.0),
        ];
        assert_eq!(
            collapse_params(cmds),
            vec![load(8), p(8, 2, 0.5), p(4, 1, -7.0), p(8, 1, -3.0), load(8), p(8, 1, -5.0)]
        );
    }

    #[test]
    fn a_version_1_journal_still_replays() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("journal.bin");
        {
            let mut f = File::create(&path).unwrap();
            write_frame(&mut f, &Envelope { version: 1, id: 0, body: set(1) }).unwrap();
        }
        let (_journal, replay) = Journal::open(&path).unwrap();
        assert_eq!(replay, vec![set(1)]);
        assert!(fs::metadata(&path).unwrap().len() > 0, "not truncated");
    }

    /// The journal keeps its own format version, so an API bump does not stop
    /// an older engine from reading it (rolling back must not lose routes).
    #[test]
    fn records_are_written_in_the_journal_format_version() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("journal.bin");
        {
            let (mut j, _) = Journal::open(&path).unwrap();
            j.append(&set(1)).unwrap();
            j.compact(&[set(2)]).unwrap();
            j.append(&set(3)).unwrap();
        }
        let mut f = BufReader::new(File::open(&path).unwrap());
        let mut n = 0;
        while let Some(env) = confluence_api::read_frame::<_, Envelope<Command>>(&mut f).unwrap() {
            assert_eq!(env.version, JOURNAL_VERSION);
            n += 1;
        }
        assert_eq!((n, JOURNAL_VERSION), (2, 1));
    }

    #[test]
    fn appended_commands_replay_in_order() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("j.bin");
        {
            let (mut j, replay) = Journal::open(&path).unwrap();
            assert!(replay.is_empty());
            j.append(&set(1)).unwrap();
            j.append(&Command::RemovePoint { input: 1, output: 1 }).unwrap();
        }
        let (_, replay) = Journal::open(&path).unwrap();
        assert_eq!(replay, vec![set(1), Command::RemovePoint { input: 1, output: 1 }]);
    }

    #[test]
    fn torn_tail_is_discarded_and_appending_continues() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("j.bin");
        {
            let (mut j, _) = Journal::open(&path).unwrap();
            j.append(&set(1)).unwrap();
            j.append(&set(2)).unwrap();
        }
        let len = fs::metadata(&path).unwrap().len();
        OpenOptions::new().write(true).open(&path).unwrap().set_len(len - 2).unwrap();
        {
            let (mut j, replay) = Journal::open(&path).unwrap();
            assert_eq!(replay, vec![set(1)]);
            j.append(&set(3)).unwrap();
        }
        let (_, replay) = Journal::open(&path).unwrap();
        assert_eq!(replay, vec![set(1), set(3)]);
    }

    #[test]
    fn compact_replaces_history_atomically() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("j.bin");
        let (mut j, _) = Journal::open(&path).unwrap();
        for i in 0..10 {
            j.append(&set(i)).unwrap();
        }
        j.compact(&[set(9)]).unwrap();
        j.append(&set(10)).unwrap();
        drop(j);
        let (_, replay) = Journal::open(&path).unwrap();
        assert_eq!(replay, vec![set(9), set(10)]);
    }

    #[test]
    fn a_journal_has_a_single_writer() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("j.bin");
        let (mut first, _) = Journal::open(&path).unwrap();
        let err = Journal::open(&path).err().expect("second open must fail while the first is alive");
        assert!(err.to_string().contains("in use"), "{err}");
        first.append(&set(1)).unwrap();
        first.compact(&[set(1)]).unwrap();
        drop(first);
        let (_, replay) = Journal::open(&path).unwrap();
        assert_eq!(replay, vec![set(1)], "released on drop, and compaction still works under the lock");
    }
}
