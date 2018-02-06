use super::{DiskError, FileSystemType, PartitionChange as Change, PartitionFlag};
use super::FileSystemType::*;
use std::fs::OpenOptions;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

pub struct Coordinates {
    start: u64,
    end:   u64,
}

impl Coordinates {
    pub fn new(start: u64, end: u64) -> Coordinates { Coordinates { start, end } }
}

pub struct ResizeOperation {
    sector_size: u64,
    old:         Coordinates,
    new:         Coordinates,
}

struct Offset {
    offset:  i64,
    inner:   OffsetCoordinates,
    overlap: Option<OffsetCoordinates>,
}

struct OffsetCoordinates {
    skip:   u64,
    length: u64,
}

const MEBIBYTE: u64 = 1_048_576;
const MEGABYTE: u64 = 1_000_000;

impl ResizeOperation {
    pub fn new(sector_size: u64, old: Coordinates, new: Coordinates) -> ResizeOperation {
        ResizeOperation {
            sector_size,
            old,
            new,
        }
    }

    /// Note that the `end` field in the coordinates are actually length values.
    fn offset(&self) -> Offset {
        let offset = self.new.start as i64 - self.old.start as i64;
        if self.new.start > self.old.start {
            Offset {
                offset,
                inner: OffsetCoordinates {
                    skip:   self.old.start,
                    length: offset as u64,
                },
                overlap: Some(OffsetCoordinates {
                    skip:   self.new.start,
                    length: self.old.start - self.new.start,
                }),
            }
        } else {
            Offset {
                offset,
                inner: OffsetCoordinates {
                    skip:   self.old.start,
                    length: self.old.end - self.old.start,
                },
                overlap: None,
            }
        }
    }

    fn is_shrinking(&self) -> bool { self.relative_sectors() < 0 }

    fn is_growing(&self) -> bool { self.relative_sectors() > 0 }

    fn is_moving(&self) -> bool { self.old.start != self.new.start }

    fn absolute_sectors(&self) -> u64 { (self.new.end - self.new.start) }

    fn relative_sectors(&self) -> i64 {
        // Obtain the differences between the start and end sectors.
        let diff_start = self.new.start as i64 - self.old.start as i64;
        let diff_end = self.new.end as i64 - self.old.end as i64;

        if diff_start == 0 {
            diff_end
        } else if diff_start == diff_end {
            0
        } else {
            diff_end - diff_start
        }
    }

    fn as_absolute_mebibyte(&self) -> u64 { self.absolute_sectors() * self.sector_size / MEBIBYTE }

    fn as_absolute_megabyte(&self) -> u64 { self.absolute_sectors() * self.sector_size / MEGABYTE }

    fn as_relative_megabyte(&self) -> i64 {
        self.relative_sectors() * self.sector_size as i64 / MEGABYTE as i64
    }

    fn as_relative_mebibyte(&self) -> i64 {
        self.relative_sectors() * self.sector_size as i64 / MEBIBYTE as i64
    }
}

#[allow(dead_code)]
enum ResizeUnit {
    AbsoluteMebibyte,
    AbsoluteMegabyte,
    AbsoluteSectors,
    RelativeMebibyte,
    RelativeMegabyte,
    RelativeSectors,
}

// TODO: Write tests for this function.

pub(crate) fn resize<DELETE, CREATE>(
    change: Change,
    resize: ResizeOperation,
    mut delete: DELETE,
    mut create: CREATE,
) -> Result<(), DiskError>
where
    DELETE: FnMut(u32) -> Result<(), DiskError>,
    CREATE: FnMut(u64, u64, Option<FileSystemType>, &[PartitionFlag]) -> Result<PathBuf, DiskError>,
{
    let moving = resize.is_moving();
    let shrinking = resize.is_shrinking();
    let growing = resize.is_growing();

    // Create the command and its arguments based on the file system to apply.
    // TODO: Handle the unimplemented file systems.
    let (cmd, args, unit): (&str, &[&'static str], ResizeUnit) = match change.filesystem {
        // Some(Btrfs) => ("btrfs", &["filesystem", "resize"], false),
        Some(Ext2) | Some(Ext3) | Some(Ext4) => ("resize2fs", &[], ResizeUnit::AbsoluteMebibyte),
        // Some(Exfat) => (),
        // Some(F2fs) => ("resize.f2fs"),
        // Some(Fat16) | Some(Fat32) => ("fatresize", &["-s"], true),
        // Some(Ntfs) => ("ntfsresize", &["-s"], true),
        Some(Swap) => unreachable!("Disk::diff() handles this"),
        // Some(Xfs) => ("xfs_growfs"),
        fs => unimplemented!("{:?} handling", fs),
    };

    let size = match unit {
        ResizeUnit::AbsoluteMebibyte => format!("{}M", resize.as_absolute_mebibyte()),
        ResizeUnit::AbsoluteMegabyte => format!("{}M", resize.as_absolute_megabyte()),
        ResizeUnit::AbsoluteSectors => format!("{}", resize.absolute_sectors()),
        ResizeUnit::RelativeMebibyte => format!("{}M", resize.as_relative_mebibyte()),
        ResizeUnit::RelativeMegabyte => format!("{}M", resize.as_relative_megabyte()),
        ResizeUnit::RelativeSectors => format!("{}", resize.relative_sectors()),
    };

    // TODO: If the partition is going to be reformatted, distinst should just delete & recreate.
    // TODO: Grow file system in partition table before attempting to resize it.
    // TODO: If growing & moving, move the partition first.

    let mut partition = change.path;

    if shrinking {
        info!("libdistinst: shrinking {}", partition.display());
        resize_(cmd, args, &size, &partition).map_err(|why| DiskError::PartitionResize { why })?;
    } else if growing {
        info!("libdistinst: growing {}", partition.display());
        delete(change.num as u32)?;
        partition = create(
            resize.new.start,
            resize.new.end,
            change.filesystem,
            &change.flags,
        )?;
        resize_(cmd, args, &size, &partition).map_err(|why| DiskError::PartitionResize { why })?;
    }

    // If the partition is to be moved, then we will ensure that it has been deleted,
    // and then dd will be used to move the partition before recreating it in the table.
    if moving {
        delete(change.num as u32)?;
        dd(&change.device_path, resize.offset(), change.sector_size)
            .map_err(|why| DiskError::PartitionMove { why })?;

        // NOTE: This should return a new partition path.
        create(
            resize.new.start,
            resize.new.end,
            change.filesystem,
            &change.flags,
        )?;
    }
    Ok(())
}

fn dd<P: AsRef<Path>>(path: P, offset: Offset, bs: u64) -> io::Result<()> {
    fn dd_op<P: AsRef<Path>>(
        path: P,
        offset: i64,
        coords: OffsetCoordinates,
        bs: u64,
    ) -> io::Result<()> {
        info!(
            "libdistinst: moving partition on {} with {} block size: {{ skip: {}; offset: {}; \
             length: {} }}",
            path.as_ref().display(),
            bs as usize,
            coords.skip,
            offset,
            coords.length
        );

        let mut disk = OpenOptions::new().read(true).write(true).open(&path)?;
        let mut buffer = vec![0; bs as usize];

        for index in 0..coords.length {
            let input = bs * coords.skip + index;
            let offset = (input as i64 + (bs as i64 * offset)) as u64;

            disk.seek(SeekFrom::Start(input))?;
            disk.read_exact(&mut buffer[..bs as usize])?;

            disk.seek(SeekFrom::Start(offset))?;
            disk.write(&mut buffer[..bs as usize])?;
        }

        disk.sync_all()
    }

    if let Some(excess) = offset.overlap {
        dd_op(&path, offset.offset, excess, bs)?;
    }

    dd_op(path, offset.offset, offset.inner, bs)?;

    Ok(())
}

fn resize_<P: AsRef<Path>>(cmd: &str, args: &[&str], size: &str, path: P) -> io::Result<()> {
    info!(
        "libdistinst: resizing {} to {}",
        path.as_ref().display(),
        size
    );

    let mut resize_cmd = Command::new(cmd);
    if !args.is_empty() {
        resize_cmd.args(args);
    }
    resize_cmd.arg(path.as_ref());
    resize_cmd.arg(size);

    eprintln!("{:?}", resize_cmd);

    // for attempt in 0..3 {
    //     ::std::thread::sleep(::std::time::Duration::from_secs(1));
    //     let result = blockdev(&path, &["--flushbufs", "--rereadpt"]);
    //     if result.is_err() && attempt == 2 {
    //         result.map_err(|why| DiskError::DiskSync { why })?
    //     } else {
    //         break;
    //     }
    // }

    fsck(&path).and_then(|_| {
        let status = resize_cmd.stdout(Stdio::null()).status()?;
        if status.success() {
            Ok(())
        } else {
            Err(io::Error::new(
                io::ErrorKind::Other,
                format!(
                    "resize for {:?} failed with status: {}",
                    path.as_ref().display(),
                    status
                ),
            ))
        }
    })
}

fn fsck<P: AsRef<Path>>(part: P) -> io::Result<()> {
    let part = part.as_ref();
    let status = Command::new("fsck")
        .arg("-fy")
        .arg(part)
        .stdout(Stdio::null())
        .status()?;
    if status.success() {
        info!("libdistinst: performed fsck on {}", part.display());
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::Other,
            format!("fsck on {} failed with status: {}", part.display(), status),
        ))
    }
}
