#![deny(rust_2018_idioms, unused, unused_crate_dependencies, unused_import_braces, unused_lifetimes, unused_qualifications, warnings)]
#![forbid(unsafe_code)]

use {
    std::{
        collections::BTreeMap,
        ffi::OsString,
        path::Path,
        pin::{
            Pin,
            pin,
        },
        time::Duration,
    },
    bytesize::ByteSize,
    chrono::prelude::*,
    futures::{
        future::Future,
        stream::TryStreamExt as _,
    },
    itertools::Itertools as _,
    lazy_regex::regex_captures,
    systemd_minecraft::World,
    systemstat::{
        Platform as _,
        System,
    },
    tokio::{
        process::Command,
        time::sleep,
    },
    wheel::{
        fs,
        traits::{
            AsyncCommandOutputExt as _,
            IoResultExt as _,
        },
    },
};

const BACKUP_PATH: &str = "/media/backup/world";
const TIMESTAMP_FORMAT: &str = "%Y-%m-%d_%H-%M-%S";

#[derive(Debug, thiserror::Error)]
enum Error {
    #[error(transparent)] ChronoParse(#[from] chrono::format::ParseError),
    #[error(transparent)] Minecraft(#[from] systemd_minecraft::Error),
    #[error(transparent)] Wheel(#[from] wheel::Error),
    #[error("not enough room to create a backup")]
    DiskSpace,
    #[error("found file in backup path not matching the filename format")]
    FilenameFormat,
    #[error("unexpected minecraft_server.jar filename format")]
    JarPath,
    #[error("failed to check file system stats at backup directory")]
    NoMount,
    #[error("non-UTF-8 filename")]
    OsString(OsString),
    #[error("non-UTF-8 filename")]
    Utf8,
}

impl From<OsString> for Error {
    fn from(value: OsString) -> Self {
        Self::OsString(value)
    }
}

//FROM https://docs.rs/fs_extra/1.3.0/src/fs_extra/dir.rs.html#786-816 modified to be async and use ByteSize
fn dir_size(path: impl AsRef<Path>) -> Pin<Box<dyn Future<Output = wheel::Result<ByteSize>>>> {
    let path = path.as_ref().to_owned();
    Box::pin(async {
        // Using `fs::symlink_metadata` since we don't want to follow symlinks,
        // as we're calculating the exact size of the requested path itself.
        let path_metadata = fs::symlink_metadata(&path).await?;

        let mut size_in_bytes = ByteSize::default();

        if path_metadata.is_dir() {
            let mut read_dir = pin!(fs::read_dir(path));
            while let Some(entry) = read_dir.try_next().await? {
                // `DirEntry::metadata` does not follow symlinks (unlike `fs::metadata`), so in the
                // case of symlinks, this is the size of the symlink itself, not its target.
                let entry_metadata = entry.metadata().await.at(entry.path())?; //TODO wheel

                if entry_metadata.is_dir() {
                    // The size of the directory entry itself will be counted inside the `get_size()` call,
                    // so we intentionally don't also add `entry_metadata.len()` to the total here.
                    size_in_bytes += dir_size(entry.path()).await?;
                } else {
                    size_in_bytes += entry_metadata.len();
                }
            }
        } else {
            size_in_bytes = ByteSize::b(path_metadata.len());
        }

        Ok(size_in_bytes)
    })
}

/// Deletes the backup that's closest to other backups. In case of a tie, the oldest backup is deleted.
///
/// If only one backup exists, it's not deleted and `false` is returned.
async fn delete_one(verbose: bool, world: &World) -> Result<bool, Error> {
    let dir = Path::new(BACKUP_PATH).join(world.to_string());
    let mut timestamps = BTreeMap::default();
    let mut entries = pin!(fs::read_dir(&dir));
    while let Some(entry) = entries.try_next().await? {
        let filename = entry.file_name().into_string()?;
        let (_, timestamp, version) = regex_captures!(r"^([0-9]{4}-[0-9]{2}-[0-9]{2}_[0-9]{2}-[0-9]{2}-[0-9]{2})_(.+?)(?:\.tar\.gz)?$", &filename).ok_or(Error::FilenameFormat)?;
        if let Ok(mut version_parts) = version.split('.').map(|part| part.parse::<i64>()).try_collect::<_, Vec<_>, _>() {
            version_parts.resize(3, 0);
            let [major, minor, patch] = <[_; 3]>::try_from(version_parts).unwrap();
            timestamps.insert((major, minor, patch, Utc.datetime_from_str(timestamp, TIMESTAMP_FORMAT)?), filename);
        } else {
            return Err(Error::FilenameFormat)
        }
    }
    let filename = match timestamps.len() {
        0 | 1 => return Ok(false),
        2 => timestamps.into_values().next().unwrap(),
        _ => timestamps.iter().tuple_windows().min_by_key(|&((&prev, _), (&curr, _), (&next, _))| {
            fn distance([(old_major, old_minor, old_patch, old_time), (new_major, new_minor, new_patch, new_time)]: [(i64, i64, i64, DateTime<Utc>); 2]) -> (i64, i64, i64, chrono::Duration) {
                let major_distance = new_major - old_major;
                let minor_distance = if new_major == old_major { new_minor - old_minor } else { 0 };
                let patch_distance = if new_major == old_major && new_minor == old_minor { new_patch - old_patch } else { 0 };
                (major_distance, minor_distance, patch_distance, new_time - old_time)
            }

            let mut distances = [distance([prev, curr]), distance([curr, next])];
            distances.sort();
            distances
        }).unwrap().1.1.clone(),
    };
    if verbose {
        println!("deleting {filename}");
    }
    let path = dir.join(filename);
    if fs::symlink_metadata(&path).await?.is_dir() {
        fs::remove_dir_all(path).await?;
    } else {
        fs::remove_file(path).await?;
    }
    Ok(true)
}

async fn make_backup(verbose: bool, world: &World) -> Result<(), Error> {
    let jar_path = world.dir().join("minecraft_server.jar");
    let jar_path = fs::read_link(&jar_path).await?;
    let now = Utc::now();
    let (_, version) = jar_path.file_stem().ok_or(Error::JarPath)?.to_str().ok_or(Error::Utf8)?.split_once('.').ok_or(Error::JarPath)?;
    if verbose {
        println!("backing up {world} world");
    }
    loop {
        let output = Command::new("rsync")
            .arg("--delete")
            .arg("--archive")
            .arg("--itemize-changes")
            .arg(world.dir())
            .arg(Path::new(BACKUP_PATH).join(world.to_string()).join(format!("{}_{}", now.format(TIMESTAMP_FORMAT), version)))
            .check("rsync").await?;
        if output.stdout.is_empty() { break }
    }
    Ok(())
}

async fn compress_all(verbose: bool, world: &World) -> Result<(), Error> {
    let dir = Path::new(BACKUP_PATH);

    'outer: loop {
        let mut entries = pin!(fs::read_dir(dir));
        let mut smallest_uncompressed = None;
        while let Some(entry) = entries.try_next().await? {
            let path = entry.path();
            let mut entries = pin!(fs::read_dir(path));
            while let Some(entry) = entries.try_next().await? {
                let path = entry.path();
                if entry.file_type().await.at(&path)?.is_dir() {
                    let size = dir_size(&path).await?;
                    if smallest_uncompressed.as_ref().map_or(true, |&(_, smallest_size)| size < smallest_size) {
                        smallest_uncompressed = Some((path, size));
                    }
                }
            }
        }
        let Some((path, size)) = smallest_uncompressed else { break };
        let Some(filename) = path.file_name() else { panic!("backup at root") };
        let parent = path.parent().unwrap();
        while dir.ancestors().map(|ancestor| System::new().mount_at(ancestor)).find_map(Result::ok).ok_or(Error::NoMount)?.avail < size {
            // not enough room to compress anything, delete backups to make room
            if !delete_one(verbose, world).await? { return Err(Error::DiskSpace) }
            if !fs::exists(&path).await? { continue 'outer }
        }
        if verbose {
            println!("compressing {}", filename.to_string_lossy());
        }
        Command::new("tar")
            .arg(if verbose { "-cvzf" } else { "-czf" })
            .arg(format!("{}.tar.gz", filename.to_str().ok_or(Error::Utf8)?))
            .arg(filename)
            .current_dir(parent)
            .check("tar").await?;
        fs::remove_dir_all(path).await?;
    }
    Ok(())
}

/// Backups will be deleted until:
///
/// * at least `amount` gibibytes are free _and_ at least `amount` % of the disk is free (returns `Ok(true)`),
/// * only one backup file is remaining (returns `Ok(false)`), or
/// * an error occurs (returns `Err(_)`).
async fn make_room(amount: ByteSize, verbose: bool, world: &World) -> Result<bool, Error> {
    let dir = Path::new(BACKUP_PATH);
    while dir.ancestors().map(|ancestor| System::new().mount_at(ancestor)).find_map(Result::ok).ok_or(Error::NoMount)?.avail < amount {
        if !delete_one(verbose, world).await? { return Ok(false) }
    }
    Ok(true)
}

#[derive(clap::Parser)]
#[clap(version)]
struct Args {
    #[clap(short, long)]
    verbose: bool,
    #[clap(default_value = "wurstmineberg")]
    world: String,
}

async fn do_backup(verbose: bool, world: &World) -> Result<(), Error> {
    let world_size = dir_size(world.dir()).await?;
    if make_room(world_size, verbose, world).await? {
        make_backup(verbose, world).await?;
        compress_all(verbose, world).await?;
        Ok(())
    } else {
        Err(Error::DiskSpace)
    }
}

#[wheel::main(debug)]
async fn main(Args { verbose, world }: Args) -> Result<(), Error> {
    let world = World::new(world);
    world.command("save-off").await?;
    world.command("save-all").await?;
    sleep(Duration::from_secs(10)).await;
    let res = do_backup(verbose, &world).await;
    let save_on_res = world.command("save-on").await.map(|_| ()).map_err(Error::from); // reenable saves even if backup failed
    res.and(save_on_res)
}
