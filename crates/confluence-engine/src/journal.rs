//! Crash-safe persistence of mutating commands.
//!
//! The journal is a file of framed `Envelope<Command>`s. On open it is replayed;
//! a torn final frame (crash mid-write) is cut off. `compact` rewrites it
//! atomically (temp file + rename) as the minimal commands for the current state.

use std::fs::{self, File, OpenOptions};
use std::io::{self, BufReader, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use confluence_api::{read_envelope, write_frame, Command, Envelope, FrameError};

pub struct Journal {
    path: PathBuf,
    file: File,
    next_id: u32,
}

impl Journal {
    /// Opens (creating if needed) the journal and returns it with the commands
    /// to replay, oldest first.
    pub fn open(path: &Path) -> io::Result<(Journal, Vec<Command>)> {
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir)?;
        }
        let mut file = OpenOptions::new().read(true).write(true).create(true).truncate(false).open(path)?;
        let mut commands = Vec::new();
        let mut good_len = 0u64;
        {
            let mut reader = BufReader::new(&mut file);
            loop {
                match read_envelope::<_, Command>(&mut reader) {
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
        Ok((Journal { path: path.to_path_buf(), file, next_id }, commands))
    }

    /// Appends a mutating command and flushes it to the OS.
    pub fn append(&mut self, cmd: &Command) -> io::Result<()> {
        write_frame(&mut self.file, &Envelope::new(self.next_id, cmd.clone())).map_err(io::Error::other)?;
        self.next_id = self.next_id.wrapping_add(1);
        Ok(())
    }

    /// Atomically replaces the journal with `state` (e.g. one SetPoint per live point).
    pub fn compact(&mut self, state: &[Command]) -> io::Result<()> {
        let tmp = self.path.with_extension("tmp");
        {
            let mut out = File::create(&tmp)?;
            for (i, cmd) in state.iter().enumerate() {
                write_frame(&mut out, &Envelope::new(i as u32, cmd.clone())).map_err(io::Error::other)?;
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
}
