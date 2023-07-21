use anyhow::Result;
use archiver::PayloadMetadata;
use binrw::BinRead;
use paris::{error, info, success, warn};
use std::fs::File;

#[cfg(unix)]
use std::{fs::Permissions, os::unix::prelude::PermissionsExt};

use std::io::{prelude::Write, Cursor};
use std::path::{Path, PathBuf};
use std::process::exit;
use std::{env, fs};

mod archiver;
mod erlang_launcher;
mod errors;

use crate::archiver::{FoilzFileRecord, FoilzPayload};
use crate::errors::WrapperError;

pub const IS_PROD: bool = !option_env!("IS_PROD").is_none();
pub const RELEASE_NAME: &str = env!("RELEASE_NAME");
pub const RELEASE_METADATA_STR: &str = env!("RELEASE_METADATA");

pub const INSTALL_SUFFIX: &str = ".burrito";

// Simple macro to only run a code block if IS_PROD is false
macro_rules! if_debug {
    ($body:block) => {
        if (!IS_PROD) {
            $body
        }
    };
}

fn main() {
    // If on windows, enable ANSI support so we can have color output
    // Windows 10+ only
    if cfg!(windows) {
        let _ = enable_ansi_support::enable_ansi_support();
    }

    // This flag is used later on determine if we need to install or not
    // the value reports
    #[allow(unused_assignments)]
    let mut needs_install = false;

    let args: Vec<String> = env::args().skip(1).collect();
    if_debug!({
        info!("IS_PROD={}", IS_PROD);
        info!("RELEASE_NAME={}", RELEASE_NAME);
        info!("ARGS={:?}", args);
        info!("METADATA_STRING={}", RELEASE_METADATA_STR);
    });

    // Bit of a nasty hack to get a mutable global metadata struct...
    // Probably a better way to do this!
    let release_meta = match maybe_parse_metadata() {
        Ok(meta) => meta,
        Err(err) => {
            error!("Error parsing metadata: {}", err);
            exit(1);
        }
    };

    // Compute base install directory
    let mut base_install_dir = match get_base_install_dir() {
        Ok(base_dir) => base_dir,
        Err(err) => {
            error!("Error computing the base install directory: {}", err);
            exit(1);
        }
    };

    // Compute full install directory
    push_final_install_dir(&mut base_install_dir, &release_meta);

    // If the directory does not exist, we need to install
    needs_install = determine_needs_install(&base_install_dir);

    if_debug!({
        info!("INSTALL_DIR={}", base_install_dir.display());
        info!("NEEDS_INSTALL={}", needs_install);
        warn!("HEADS UP: We're ALWAYS going to re-install in debug mode!");
        needs_install = true;
    });

    // If we need to install, un-compress, and  and unpack the payload to disk
    if needs_install {
        let result: Result<(), anyhow::Error> = decompress_payload(&base_install_dir);
        if result.is_err() {
            error!("Error unpacking payload: {}", result.unwrap_err());
            exit(1);
        }
    }

    // Now launch!
    match erlang_launcher::launch_app(&base_install_dir, &release_meta, &args) {
        Ok(()) => {}
        Err(err) => {
            error!("Failed to launch inner application: {}", err);
            exit(1);
        }
    }
}

fn maybe_parse_metadata() -> Result<PayloadMetadata> {
    let metadata: PayloadMetadata =
        serde_json::from_str(RELEASE_METADATA_STR).or(Err(WrapperError::MetadataCorrupted))?;
    Ok(metadata)
}

fn decompress_payload(destination_path: &Path) -> Result<()> {
    // Embed and decompress payload
    // Payload is present at compile time, it's generated by the `build.rs` file in the top level of the crate
    let payload = include_bytes!("../payload.foilz.xz");
    let mut decompressor = snap::raw::Decoder::new();
    let decompressed_data = decompressor
        .decompress_vec(payload)
        .or(Err(WrapperError::PayloadDecompressFailed))?;

    // Read the decompressed stream into structs
    let parsed_payload: FoilzPayload = FoilzPayload::read_be(&mut Cursor::new(&decompressed_data))
        .or(Err(WrapperError::PayloadDecompressFailed))?;

    // Write each record to disk
    for record in parsed_payload.files {
        write_payload_file(&record, &destination_path)?;
    }

    if_debug!({
        success!(
            "Finished payload decompression! Uncompressed size: {}",
            decompressed_data.len()
        );
    });

    Ok(())
}

fn write_payload_file(
    record: &FoilzFileRecord,
    destination_path: &Path,
) -> Result<(), WrapperError> {
    // Compute full destination path of this file
    let mut full_path: PathBuf = destination_path.clone().to_path_buf();
    let dest_name: PathBuf = Path::new(&record.file_path.to_string()).to_path_buf();
    full_path.push(dest_name);

    // Compute parent path of install
    let parent_path = full_path
        .parent()
        .ok_or(WrapperError::ExtractInvalidInstallDir)?;

    // Create all directories needed for placing this file
    fs::create_dir_all(parent_path).or(Err(WrapperError::ExtractMkdirFailed(
        "Could not create all install directories".to_owned(),
    )))?;

    // Create the file
    let mut new_file = File::create(&full_path).or(Err(WrapperError::ExtractFileWriteFailed(
        "Could not create file".to_owned(),
    )))?;

    // Write file data
    new_file
        .write_all(&record.file_data)
        .or(Err(WrapperError::ExtractFileWriteFailed(
            "Could not write data to file".to_owned(),
        )))?;

    if_debug!({
        success!("Wrote File: {}", full_path.display());
    });

    // Set file mode if NOT on Windows
    #[cfg(unix)]
    {
        let perm: Permissions = Permissions::from_mode(record.file_mode);
        match new_file.set_permissions(perm) {
            Ok(it) => it,
            Err(err) => return Err(WrapperError::ExtractChmodFailed(err.to_string())),
        };

        if_debug!({
            info!(
                "\tSet perm {:#o} -> {}",
                record.file_mode,
                full_path.display()
            );
        });
    }

    Ok(())
}

fn determine_needs_install(full_install_dir: &Path) -> bool {
    !full_install_dir.exists()
}

fn push_final_install_dir(base_install_dir: &mut PathBuf, release_meta: &PayloadMetadata) {
    let install_suffix = format!(
        "{}_erts-{}_{}",
        RELEASE_NAME, release_meta.erts_version, release_meta.app_version
    );
    base_install_dir.push(install_suffix);
}

fn get_base_install_dir() -> Result<PathBuf, WrapperError> {
    let default_base_dir = dirs::data_dir();
    let install_dir_env_name = format!("{}_INSTALL_PATH", RELEASE_NAME);
    let possible_env_override = match env::var(install_dir_env_name) {
        Ok(val) => Some(val),
        Err(_e) => None,
    };

    if possible_env_override.is_some() {
        info!(
            "Install path is being overridden using env var: {}_INSTALL_PATH",
            RELEASE_NAME
        );
        info!("New install path is: {:?}", possible_env_override);
        let mut path = Path::new(&possible_env_override.unwrap()).to_path_buf();
        path.push(INSTALL_SUFFIX);
        Ok(path)
    } else {
        if default_base_dir.is_none() {
            return Err(WrapperError::ExtractCannotComputeInstallDir);
        }

        let mut path = default_base_dir.unwrap().to_path_buf();
        path.push(INSTALL_SUFFIX);
        Ok(path)
    }
}