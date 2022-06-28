//! See <https://github.com/matklad/cargo-xtask/>.

mod arch;
mod flags;

use std::{
	env::{self, VarError},
	ffi::OsStr,
	path::{Path, PathBuf},
};

use anyhow::{anyhow, Result};
use arch::Arch;
use goblin::{archive::Archive, elf64::header};
use llvm_tools::LlvmTools;
use xshell::{cmd, Shell};

fn main() -> Result<()> {
	flags::Xtask::from_env()?.run()
}

impl flags::Xtask {
	fn run(self) -> Result<()> {
		match self.subcommand {
			flags::XtaskCmd::Help(_) => {
				println!("{}", flags::Xtask::HELP);
				Ok(())
			}
			flags::XtaskCmd::Build(build) => build.run(),
			flags::XtaskCmd::Clippy(clippy) => clippy.run(),
		}
	}
}

impl flags::Build {
	fn run(self) -> Result<()> {
		let sh = sh()?;

		eprintln!("Building kernel");
		cmd!(sh, "cargo build")
			.env("CARGO_ENCODED_RUSTFLAGS", self.cargo_encoded_rustflags()?)
			.args(self.arch.cargo_args())
			.args(self.target_dir_args())
			.args(self.no_default_features_args())
			.args(self.features_args())
			.args(self.profile_args())
			.run()?;

		let build_archive = self.build_archive();
		let dist_archive = self.dist_archive();
		eprintln!(
			"Copying {} to {}",
			build_archive.display(),
			dist_archive.display()
		);
		sh.create_dir(dist_archive.parent().unwrap())?;
		sh.copy_file(&build_archive, &dist_archive)?;

		eprintln!("Setting OSABI");
		self.set_osabi()?;

		eprintln!("Exporting symbols");
		self.export_syms()?;

		eprintln!("Kernel available at {}", self.dist_archive().display());
		Ok(())
	}

	fn cargo_encoded_rustflags(&self) -> Result<String> {
		let outer_rustflags = match env::var("CARGO_ENCODED_RUSTFLAGS") {
			Ok(s) => Some(s),
			Err(VarError::NotPresent) => None,
			Err(err) => return Err(err.into()),
		};
		let mut rustflags = outer_rustflags
			.as_deref()
			.map(|s| vec![s])
			.unwrap_or_default();

		// TODO: Re-enable mutable-noalias
		// https://github.com/hermitcore/libhermit-rs/issues/200
		rustflags.push("-Zmutable-noalias=no");

		if self.instrument_mcount {
			rustflags.push("-Zinstrument-mcount");
		}

		rustflags.extend(self.arch.rustflags());

		Ok(rustflags.join("\x1f"))
	}

	fn target_dir_args(&self) -> [&OsStr; 2] {
		["--target-dir".as_ref(), self.target_dir().as_ref()]
	}

	fn no_default_features_args(&self) -> &[&str] {
		if self.no_default_features {
			&["--no-default-features"]
		} else {
			&[]
		}
	}

	fn features_args(&self) -> impl Iterator<Item = &str> {
		self.features
			.iter()
			.flat_map(|feature| ["--features", feature.as_str()])
	}

	fn profile_args(&self) -> [&str; 2] {
		["--profile", self.profile()]
	}

	fn set_osabi(&self) -> Result<()> {
		let sh = sh()?;
		let archive_path = self.dist_archive();
		let mut archive_bytes = sh.read_binary_file(&archive_path)?;
		let archive = Archive::parse(&archive_bytes)?;

		let file_offsets = (0..archive.len())
			.map(|i| archive.get_at(i).unwrap().offset)
			.collect::<Vec<_>>();

		for file_offset in file_offsets {
			let file_offset = usize::try_from(file_offset).unwrap();
			archive_bytes[file_offset + header::EI_OSABI] = header::ELFOSABI_STANDALONE;
		}

		sh.write_file(&archive_path, archive_bytes)?;

		Ok(())
	}

	fn export_syms(&self) -> Result<()> {
		let archive = self.dist_archive();

		let syscall_symbols = syscall_symbols(&archive)?;
		let explicit_exports = [
			"_start",
			"__bss_start",
			"runtime_entry",
			// lwIP functions (C runtime)
			"init_lwip",
			"lwip_read",
			"lwip_write",
		]
		.into_iter();

		let symbol_redefinitions =
			explicit_exports.chain(syscall_symbols.iter().map(String::as_str));

		retain_symbols(&archive, symbol_redefinitions)?;

		Ok(())
	}

	fn profile(&self) -> &str {
		self.profile
			.as_deref()
			.unwrap_or(if self.release { "release" } else { "dev" })
	}

	fn target_dir(&self) -> &Path {
		self.target_dir
			.as_deref()
			.unwrap_or_else(|| Path::new("target"))
	}

	fn out_dir(&self) -> PathBuf {
		let mut out_dir = self.target_dir().to_path_buf();
		out_dir.push(self.arch.triple());
		out_dir.push(match self.profile() {
			"dev" => "debug",
			profile => profile,
		});
		out_dir
	}

	fn dist_dir(&self) -> PathBuf {
		let mut out_dir = self.target_dir().to_path_buf();
		out_dir.push(self.arch.name());
		out_dir.push(match self.profile() {
			"dev" => "debug",
			profile => profile,
		});
		out_dir
	}

	fn build_archive(&self) -> PathBuf {
		let mut built_archive = self.out_dir();
		built_archive.push("libhermit.a");
		built_archive
	}

	fn dist_archive(&self) -> PathBuf {
		let mut dist_archive = self.dist_dir();
		dist_archive.push("libhermit.a");
		dist_archive
	}
}

impl flags::Clippy {
	fn run(self) -> Result<()> {
		let sh = sh()?;

		// TODO: Enable clippy for aarch64
		// https://github.com/hermitcore/libhermit-rs/issues/381
		#[allow(clippy::single_element_loop)]
		for target in [Arch::X86_64] {
			let target_args = target.cargo_args();
			cmd!(sh, "cargo clippy {target_args...}").run()?;
			cmd!(sh, "cargo clippy {target_args...}")
				.arg("--no-default-features")
				.run()?;
			cmd!(sh, "cargo clippy {target_args...}")
				.arg("--all-features")
				.run()?;
		}

		cmd!(sh, "cargo clippy --package xtask").run()?;

		Ok(())
	}
}

fn syscall_symbols(archive: &Path) -> Result<Vec<String>> {
	let sh = sh()?;

	let archive_bytes = sh.read_binary_file(archive)?;
	let archive = Archive::parse(&archive_bytes)?;
	let symbols = archive
		.summarize()
		.into_iter()
		.filter(|(member_name, _, _)| member_name.starts_with("hermit-"))
		.flat_map(|(_, _, symbols)| symbols)
		.filter(|symbol| symbol.starts_with("sys_"))
		.map(String::from)
		.collect();

	Ok(symbols)
}

fn retain_symbols<'a>(archive: &Path, symbols: impl Iterator<Item = &'a str>) -> Result<()> {
	let sh = sh()?;

	let symbol_renames = symbols
		.map(|symbol| format!("hermit_{symbol} {symbol}\n"))
		.collect::<String>();

	let rename_path = archive.with_extension("redefine-syms");
	sh.write_file(&rename_path, &symbol_renames)?;

	let objcopy = binutil("objcopy")?;
	cmd!(sh, "{objcopy} --prefix-symbols=hermit_ {archive}").run()?;
	cmd!(sh, "{objcopy} --redefine-syms={rename_path} {archive}").run()?;

	sh.remove_path(&rename_path)?;

	Ok(())
}

fn binutil(name: &str) -> Result<PathBuf> {
	let exe_suffix = env::consts::EXE_SUFFIX;
	let exe = format!("llvm-{name}{exe_suffix}");

	let path = LlvmTools::new()
		.map_err(|err| anyhow!("{err:?}"))?
		.tool(&exe)
		.ok_or_else(|| anyhow!("could not find {exe}"))?;

	Ok(path)
}

fn sh() -> Result<Shell> {
	let sh = Shell::new()?;
	sh.change_dir(project_root());
	Ok(sh)
}

fn project_root() -> &'static Path {
	Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap()
}
