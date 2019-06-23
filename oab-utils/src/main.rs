extern crate clap;
extern crate libc;

use std::fmt;
use std::error::Error;
use std::fs;
use std::io;
use std::os::unix::fs::MetadataExt;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use clap::{Arg, ArgMatches, App, AppSettings, SubCommand};

// https://blog.rust-lang.org/2016/05/13/rustup.html
// https://users.rust-lang.org/t/announcing-cargo-apk/5501

#[derive(Debug, Clone)]
pub struct OabError {
    pub message: String
}

impl OabError {
    fn new(message: &str) -> Self {
        OabError {
            message: message.to_owned()
        }
    }
}

impl fmt::Display for OabError {
    fn fmt(&self, f: &mut fmt::Formatter) -> Result<(), fmt::Error> {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for OabError {
    fn description(&self) -> &str {
        &self.message
    }
}

impl From<io::Error> for OabError {
    fn from(error: io::Error) -> Self {
        OabError {
            message: error.description().to_string()
        }
    }
}

#[derive(Debug)]
struct FilePerms {
    path: std::path::PathBuf,
    permissions: u32
}

// put code related to looking up user and group ids into a trait both to
// facilitate mocking in unit tests and to isolate the os-specific code.
trait IdRetrieverModel {
    fn get_uid(&self, name: &str) -> Result<u32, OabError>;
    fn get_gid(&self, name: &str) -> Result<u32, OabError>;
}

struct IdRetriever {
}
impl IdRetrieverModel for IdRetriever {
    fn get_uid(&self, name: &str) -> Result<u32, OabError> {
        unsafe {
            let name_cstring = str_to_cstring(name)?;
            let mut buf = Vec::with_capacity(512);
            let mut passwd: libc::passwd = std::mem::zeroed();
            let mut result = std::ptr::null_mut();
            match libc::getpwnam_r(name_cstring.as_ptr(), &mut passwd,
                    buf.as_mut_ptr(), buf.capacity(), &mut result) {
                0 if !result.is_null() => Ok((*result).pw_uid),
                _ => Err(OabError {
                        message: format!("no uid found for user {}", name)
                    }
                )
            }
        }
    }

    fn get_gid(&self, name: &str) -> Result<u32, OabError> {
        unsafe {
            let name_cstring = str_to_cstring(name)?;
            // armv7-linux-androideabi doesn't have getgrnam_r so we have to
            // use getgrnam instead
            let group = libc::getgrnam(name_cstring.as_ptr());
            match group.as_ref() {
                Some(group) => Ok((*group).gr_gid),
                None => Err(OabError {
                        message: format!("no gid found for group {}", name)
                    }
                )
            }
        }
    }
}

fn get_owner_ids(path: &Path) -> Result<(u32, u32), std::io::Error> {
    let metadata = fs::metadata(path)?;
    Ok((metadata.uid(), metadata.gid()))
}

fn str_to_cstring(s: &str) -> Result<std::ffi::CString, OabError> {
    match std::ffi::CString::new(s) {
        Ok(s) => Ok(s),
        Err(e) => {
            return Err(OabError {
                    message: e.description().to_string()
                }
            )
        }
    }
}

fn parse_id_input<M: IdRetrieverModel>(id_retriever: &M, id: &str) ->
        Result<(u32, Option<u32>), OabError> {
    if let Some(index) = id.find(":") {
        let uid_str = &id[..index];
        let uid = match uid_str.parse::<u32>() {
            Ok(uid) => uid,
            Err(_) => id_retriever.get_uid(uid_str)?
        };
        let gid_str = &id[index + 1..];
        let gid = match gid_str.parse::<u32>() {
            Ok(gid) => gid,
            Err(_) => id_retriever.get_gid(gid_str)?
        };
        return Ok((uid, Some(gid)));
    }
    let uid = match id.parse::<u32>() {
        Ok(uid) => uid,
        Err(_e) => id_retriever.get_uid(id)?
    };
    Ok((uid, None))
}


fn change_owner(path: &Path, uid: u32, gid: u32) -> Result<(), OabError> {
    let path_cstring = str_to_cstring(path.to_str().unwrap())?;
    unsafe {
        match libc::chown(path_cstring.as_ptr(), uid, gid) {
            0 => Ok(()),
            _ => Err(OabError {
                    message: format!("unable to change owner of {:?} to {}:{}", path,
                        uid, gid)
                }
            )
        }
    }
}

fn change_owner_recurse(path: &Path, uid: u32, gid: u32) -> Result<(), OabError> {
    change_owner(path, uid, gid)?;
    if path.is_dir() {
        let files = path.read_dir()?;
        for file in files {
            let entry = file?;
            change_owner_recurse(&entry.path(), uid, gid)?;
        }
    }
    Ok(())
}

fn set_permissions(path: &Path, mode: u32) -> Result<(), OabError> {
    let mut perms = match fs::metadata(path) {
        Err(e) => {
            return Err(OabError {
                message: format!("error getting permissions for {:?}: {}", path,
                    e.description())
            });
        }
        Ok(metadata) => metadata.permissions()
    };
    if perms.mode() & 0o777 != mode {
        perms.set_mode(mode);
        if let Err(e) = std::fs::set_permissions(path, perms) {
            return Err(OabError {
                message: format!("unable to set mode {:o} on path {:?}: {}",
                    mode, path, e.description())
            });
        }
    }
    Ok(())
}

fn set_permissions_recurse(path: &Path, mode: u32) -> Result<(), OabError> {
    set_permissions(path, mode)?;
    if path.is_dir() {
        let files = path.read_dir()?;
        for file in files {
            let entry = file?;
            set_permissions_recurse(&entry.path(), mode)?;
        }
    }
    Ok(())
}

fn get_permissions(path: &Path) -> Result<Vec<FilePerms>, OabError>{
    let mut files_vector = Vec::with_capacity(128);
    let mut file_perms_vector = Vec::with_capacity(128);
    files_vector.push(path.to_path_buf());
    while files_vector.len() > 0 {
        if let Some(file) = files_vector.pop() {
            if file.is_dir() {
                let files = file.read_dir()?;
                files_vector.extend(files.map(|f| {
                    match f {
                        Ok(entry) => return entry.path().to_owned(),
                        Err(_) => {
                            eprintln!("error getting path from {:?}", f);
                            return std::path::PathBuf::new();
                        }
                    }
                }));
            } else {
                match fs::metadata(&file) {
                    Ok(metadata) => {
                        file_perms_vector.push(
                            FilePerms{
                                path: (&file).to_owned(),
                                permissions: metadata.permissions().mode()
                            }
                        )
                    },
                    Err(e) => return Err(OabError::new(&format!(
                        "Couldn't get metadata for file {:?}: {:?}", file, e)))
                }
            }
        }
    }
    Ok(file_perms_vector)
}

fn print_permissions_for_path(input: &Path, output_path: Option<&str>) ->
        Result<(), OabError> {
    let mut output: Box<io::Write> = match output_path {
        Some(output) => {
            if Path::new(output).is_dir() {
                return Err(OabError {
                    message: format!("error: output path {:?} is a directory",
                        output)
                });
            }
            match fs::File::create(output) {
                Ok(file) => Box::new(file),
                Err(e) => {
                    return Err(OabError {
                        message: format!("couldn't create output file {}: {}",
                            output, e)
                    });
                }
            }
        },
        None => Box::new(io::stdout())
    };

    let files = get_permissions(input)?;
    for file in files {
        let mode = file.permissions & 0o777;
        if let Some(path) = file.path.to_str() {
            if let Err(e) = output.write(format!("{} {:o}\n", path, mode).as_bytes()) {
                return Err(OabError {
                    message: format!("error writing to file {}: {}", output_path.unwrap_or("stdout"), e)
                });
            }
        } else {
            eprintln!("warning: path {:?} couldn't be displayed properly", file.path);
        }
    }
    Ok(())
}

fn setup_args<'b>() -> ArgMatches<'b> {
    // https://github.com/kbknapp/clap-rs
    let args = App::new("oab-utils")
        .setting(AppSettings::ArgRequiredElseHelp)
        .subcommand(SubCommand::with_name("owner")
            .arg(Arg::with_name("input")
                .help("file to get info from")
                .required(true)))
        .subcommand(SubCommand::with_name("get-permissions")
            .arg(Arg::with_name("path").required(true)
                .help("path to get permissions for"))
            .arg(Arg::with_name("output")
                .short("o")
                .long("output")
                .help("file to write permissions info to")
                .takes_value(true)
            )
        )
        .subcommand(SubCommand::with_name("set-permissions")
            .arg(Arg::with_name("mode")
                .help("mode to set, in octal (e.g. 644)")
                .required(true))
            .arg(Arg::with_name("recursive")
                .short("r")
                .long("recursive")
                .help("set permissions recursively"))
            .arg(Arg::with_name("input")
                .required(true))
        )
        .subcommand(SubCommand::with_name("change-owner")
            .arg(Arg::with_name("id")
                .help("uid and optionally gid to set, separated by :")
                .required(true))
            .arg(Arg::with_name("recursive")
                .short("r")
                .long("recursive")
                .help("change owner of files recursively"))
            .arg(Arg::with_name("path")
                .required(true))
        )
        .get_matches();
    args
}

fn main() {
    let args = setup_args();
    match args.subcommand() {
        ("owner", Some(args)) => {
            let input = Path::new(args.value_of("input").unwrap());
            match get_owner_ids(input) {
                Ok((uid, gid)) => {
                    println!("{{\"uid\": {}, \"gid\": {}}}", uid, gid);
                },
                Err(e) => {
                    eprintln!("error getting owner ids for {:?}: {}", input,
                        e.description());
                    std::process::exit(1);
                }
            };
        },
        ("get-permissions", Some(args)) => {
            let input = Path::new(args.value_of("path").unwrap());
            let output = args.value_of("output");
            if let Err(e) = print_permissions_for_path(input, output) {
                eprintln!("{}", e.message);
                std::process::exit(1);
            };
        },
        ("set-permissions", Some(args)) => {
            let input = Path::new(args.value_of("input").unwrap());
            let mode_str = args.value_of("mode").unwrap();
            let mode = match u32::from_str_radix(mode_str, 8) {
                Err(e) => {
                    eprintln!("error parsing input {}: {}", mode_str, e.description());
                    std::process::exit(1);
                }
                Ok(value) => value
            };
            if args.is_present("recursive") {
                if let Err(err) = set_permissions_recurse(input, mode) {
                    eprintln!("{}", err.description());
                    std::process::exit(1);
                }
            } else {
                if let Err(err) = set_permissions(input, mode) {
                    eprintln!("{}", err.description());
                    std::process::exit(1);
                }
            }
        },
        ("change-owner", Some(args)) => {
            let path = Path::new(args.value_of("path").unwrap());
            let id_retriever = IdRetriever {};
            match parse_id_input(&id_retriever, args.value_of("id").unwrap()) {
                Ok((uid, gid_option)) => {
                    let gid = match gid_option {
                        Some(gid) => gid,
                        None => match get_owner_ids(path) {
                            Ok((_, gid)) => gid,
                            Err(e) => {
                                eprintln!("unable to get group id for {:?}: {}",
                                    path, e.description());
                                std::process::exit(1);
                            }
                        }
                    };
                    if args.is_present("recursive") {
                        if let Err(e) = change_owner_recurse(path, uid, gid) {
                            eprintln!("{}", e.description());
                            std::process::exit(1);
                        }
                    } else {
                        if let Err(e) = change_owner(path, uid, gid) {
                            eprintln!("{}", e.description());
                            std::process::exit(1);
                        }
                    }
                },
                Err(e) => eprintln!("unable to parse input: {}", e.description())
            };
        },
        ("", None) => {
            eprintln!("no commands specified");
            std::process::exit(1);
        },
        _ => unreachable!()
    }
}

#[cfg(test)]
mod test_main;
