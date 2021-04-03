mod cli;
mod fs;
mod guest;
mod install;
mod network;
mod solv;

use anyhow::{anyhow, Result};
use cli::build_cli;
use solv::PackageMeta;
use std::{
    fs::File,
    io::{BufRead, BufReader},
    path::Path,
};
use bytesize::ByteSize;

const DEFAULT_MIRROR: &str = "https://repo.aosc.io/debs";

fn extract_packages(packages: &[PackageMeta], target: &Path, archive_path: &Path) -> Result<()> {
    let mut count = 0usize;
    for package in packages {
        count += 1;
        let filename = Path::new(&package.path)
            .file_name()
            .ok_or_else(|| anyhow!("Unable to determine package filename"))?;
        eprintln!(
            "[{}/{}] Extracting {} ...",
            count,
            packages.len(),
            package.name
        );
        let f = File::open(archive_path.join(filename))?;
        install::extract_deb(f, target)?;
    }

    Ok(())
}

fn collect_packages_from_lists(paths: &[&str]) -> Result<Vec<String>> {
    let mut packages = Vec::new();
    packages.reserve(1024);

    for path in paths {
        let f = File::open(path)?;
        let reader = BufReader::new(f);
        for line in reader.lines() {
            let line = line?;
            // skip comment
            if line.starts_with('#') || line.is_empty() {
                continue;
            }
            // trim whitespace
            let trimmed = line.trim();
            packages.push(trimmed.to_owned());
        }
    }

    Ok(packages)
}

#[inline]
fn collect_filenames(packages: &[PackageMeta]) -> Result<Vec<String>> {
    let mut output = Vec::new();
    for package in packages {
        output.push(
            Path::new(&package.path)
                .file_name()
                .ok_or_else(|| anyhow!("Unable to determine package filename"))?
                .to_string_lossy()
                .to_string(),
        );
    }

    Ok(output)
}

fn main() {
    let matches = build_cli().get_matches();
    let branch = matches.value_of("BRANCH").unwrap();
    let target = matches.value_of("TARGET").unwrap();
    let mirror = matches.value_of("MIRROR").unwrap_or(DEFAULT_MIRROR);
    let arches = matches.values_of("arch").unwrap().collect::<Vec<&str>>();
    let config_path = matches.value_of("config").unwrap();
    let dl_only = matches.is_present("download-only");
    let s1_only = matches.is_present("stage1-only");
    let extra_packages = matches.values_of("include");
    let extra_files = matches.values_of("include-files");
    let config = install::read_config(config_path).unwrap();
    let client = network::make_new_client().unwrap();
    let target_path = Path::new(target);
    let archive_path = target_path.join("var/cache/apt/archives");
    let mut extra_packages = if let Some(extra_packages) = extra_packages {
        extra_packages
            .map(|x| x.to_string())
            .collect::<Vec<String>>()
    } else {
        Vec::new()
    };
    if let Some(extra_files) = extra_files {
        let extras = collect_packages_from_lists(&extra_files.collect::<Vec<&str>>()).unwrap();
        eprintln!("Read {} extra packages from the lists.", extras.len());
        extra_packages.extend(extras);
    }

    std::fs::create_dir_all(target_path.join("var/lib/apt/lists")).unwrap();
    std::fs::create_dir_all(&archive_path).unwrap();
    eprintln!("Downloading manifests ...");
    let manifests =
        network::fetch_manifests(&client, mirror, branch, &arches, target_path).unwrap();
    let mut paths = Vec::new();
    for p in manifests {
        paths.push(target_path.join("var/lib/apt/lists").join(p));
    }
    eprintln!("Resolving dependencies ...");
    let mut all_stages = config.stub_packages.clone();
    all_stages.extend(config.base_packages);
    all_stages.extend(extra_packages);
    let mut pool = solv::Pool::new();
    solv::populate_pool(&mut pool, &paths).unwrap();
    let t = solv::calculate_deps(&mut pool, &all_stages).unwrap();
    let all_packages = t.create_metadata().unwrap();
    eprintln!("Total installed size: {}", ByteSize::kb(t.get_size_change().abs() as u64));
    eprintln!("Downloading packages ...");
    network::batch_download(&all_packages, mirror, &archive_path).unwrap();
    nix::unistd::sync();
    if dl_only {
        eprintln!("Download finished.");
        return;
    }

    let stub_install = solv::calculate_deps(&mut pool, &config.stub_packages)
        .unwrap()
        .create_metadata()
        .unwrap();
    eprintln!("Stage 1: Creating filesystem skeleton ...");
    std::fs::create_dir_all(target_path.join("dev")).unwrap();
    fs::bootstrap_apt(target_path, mirror, branch).unwrap();
    install::extract_bootstrap_pack(target_path).unwrap();
    fs::make_device_nodes(target_path).unwrap();
    eprintln!("Stage 1: Extracting packages ...");
    extract_packages(&stub_install, target_path, &archive_path).unwrap();
    nix::unistd::sync();
    if s1_only {
        eprintln!("Stage 1 finished.");
        return;
    }

    eprintln!("Stage 2: Installing packages ...");
    let names: Vec<String> = collect_filenames(&all_packages).unwrap();
    let script = install::write_install_script(&names, target_path).unwrap();
    let script_file = script.path().file_name().unwrap().to_string_lossy();
    guest::run_in_guest(target, &["bash", "-e", &script_file]).unwrap();
    eprintln!("Stage 2 finished.\nBase system ready!");
}