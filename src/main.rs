#[macro_use]
extern crate anyhow;
extern crate clap;

#[macro_use]
extern crate simple_error;

#[macro_use]
extern crate derive_builder;

extern crate pathdiff;

use core::sync::atomic::AtomicBool;

use std::env;
use std::os::unix::prelude::OsStrExt;
use std::str;
use std::thread;

#[macro_use]
extern crate serde_derive;

use anyhow::{Context as _, Error, Result};
use camino::{Utf8Path, Utf8PathBuf};
use indexmap::IndexSet;
use itertools::Itertools;
use signal_hook::{consts::SIGINT, iterator::Signals};

#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

mod build;
mod cli;
mod data;
mod download;
mod generate;
mod model;
mod nested_env;
mod ninja;
mod serde_bool_helpers;
mod utils;

use model::{Context, ContextBag, Dependency, Module, Rule, Task};

use generate::{get_ninja_build_file, BuildInfo, GenerateMode, GeneratorBuilder, Selector};
use nested_env::{Env, MergeOption};
use ninja::NinjaCmdBuilder;

fn determine_project_root(start: &Utf8Path) -> Result<(Utf8PathBuf, Utf8PathBuf)> {
    let mut cwd = start.to_owned();

    loop {
        let mut tmp = cwd.clone();
        tmp.push("laze-project.yml");
        if tmp.exists() {
            return Ok((cwd, Utf8PathBuf::from("laze-project.yml")));
        }
        cwd = match cwd.parent() {
            Some(p) => Utf8PathBuf::from(p),
            None => return Err(anyhow!("cannot find laze-project.yml")),
        }
    }
}

fn ninja_run(
    ninja_buildfile: &Utf8Path,
    verbose: bool,
    targets: Option<Vec<Utf8PathBuf>>,
    jobs: Option<usize>,
) -> Result<i32, Error> {
    let mut ninja_cmd = NinjaCmdBuilder::default();

    ninja_cmd
        .verbose(verbose)
        .build_file(ninja_buildfile)
        .targets(targets);

    if let Some(jobs) = jobs {
        ninja_cmd.jobs(jobs);
    }

    let ninja_exit = ninja_cmd.build().unwrap().run()?;

    match ninja_exit.code() {
        Some(code) => match code {
            0 => Ok(code),
            _ => Err(anyhow!("ninja exited with code {code}")),
        },
        None => Err(anyhow!("ninja probably killed by signal")),
    }
}

fn main() {
    let result = try_main();
    match result {
        Err(e) => {
            eprintln!("laze: error: {e:#}");
            std::process::exit(1);
        }
        Ok(code) => std::process::exit(code),
    };
}

pub static IGNORE_SIGINT: AtomicBool = AtomicBool::new(false);

fn try_main() -> Result<i32> {
    let mut signals = Signals::new([SIGINT])?;

    thread::spawn(move || {
        for sig in signals.forever() {
            if sig == SIGINT && !IGNORE_SIGINT.load(std::sync::atomic::Ordering::SeqCst) {
                std::process::exit(130);
            }
        }
    });

    let matches = cli::clap().get_matches();

    // handle completion subcommand here, so the project specific
    // stuff is skipped
    match matches.subcommand() {
        Some(("completion", matches)) => {
            fn print_completions<G: clap_complete::Generator>(gen: G, cmd: &mut clap::Command) {
                clap_complete::generate(
                    gen,
                    cmd,
                    cmd.get_name().to_string(),
                    &mut std::io::stdout(),
                );
            }
            if let Some(generator) = matches
                .get_one::<clap_complete::Shell>("generator")
                .copied()
            {
                let mut cmd = cli::clap();
                eprintln!("Generating completion file for {}...", generator);
                print_completions(generator, &mut cmd);
            }
            return Ok(0);
        }
        Some(("manpages", matches)) => {
            fn create_manpage(cmd: clap::Command, outfile: &Utf8Path) -> Result<(), Error> {
                let man = clap_mangen::Man::new(cmd);
                let mut buffer: Vec<u8> = Default::default();
                man.render(&mut buffer)?;

                std::fs::write(outfile, buffer)?;
                Ok(())
            }
            let mut outpath: Utf8PathBuf =
                matches.get_one::<Utf8PathBuf>("outdir").unwrap().clone();
            let cmd = cli::clap();

            outpath.push("laze.1");
            create_manpage(cmd.clone(), &outpath)?;

            for subcommand in cmd.get_subcommands() {
                if subcommand.is_hide_set() {
                    continue;
                }
                let name = subcommand.get_name();
                outpath.pop();
                outpath.push(format!("laze-{name}.1"));
                create_manpage(subcommand.clone(), &outpath)?;
            }

            return Ok(0);
        }
        _ => (),
    }

    if let Some(dir) = matches.get_one::<Utf8PathBuf>("chdir") {
        env::set_current_dir(dir).context(format!("cannot change to directory \"{dir}\""))?;
    }

    let cwd = Utf8PathBuf::try_from(env::current_dir()?).expect("cwd not UTF8");

    let (project_root, project_file) = determine_project_root(&cwd)?;
    let start_relpath = pathdiff::diff_utf8_paths(&cwd, &project_root).unwrap();

    println!(
        "laze: project root: {project_root} relpath: {start_relpath} project_file: {project_file}",
    );

    let global = matches.get_flag("global");
    env::set_current_dir(&project_root).context(format!("cannot change to \"{project_root}\""))?;

    match matches.subcommand() {
        Some(("build", build_matches)) => {
            let verbose = build_matches.get_count("verbose");
            let build_dir = build_matches.get_one::<Utf8PathBuf>("build-dir").unwrap();

            // collect builder names from args
            let builders = match build_matches.get_many::<String>("builders") {
                Some(values) => Selector::Some(values.cloned().collect::<IndexSet<String>>()),
                None => Selector::All,
            };

            // collect app names from args
            let apps = match build_matches.get_many::<String>("apps") {
                Some(values) => Selector::Some(values.cloned().collect::<IndexSet<String>>()),
                None => Selector::All,
            };

            let jobs = build_matches.get_one::<usize>("jobs").copied();

            let partitioner = build_matches
                .get_one::<task_partitioner::PartitionerBuilder>("partition")
                .map(|v| v.build());

            println!("laze: building {apps} for {builders}");

            // collect CLI selected/disabled modules
            let select = get_selects(build_matches);
            let disable = get_disables(build_matches);

            // collect CLI env overrides
            let cli_env = get_cli_vars(build_matches)?;

            let mode = match global {
                true => GenerateMode::Global,
                false => GenerateMode::Local(start_relpath),
            };

            let generator = GeneratorBuilder::default()
                .project_root(project_root.clone())
                .project_file(project_file)
                .build_dir(build_dir.clone())
                .mode(mode.clone())
                .builders(builders.clone())
                .apps(apps.clone())
                .select(select)
                .disable(disable)
                .cli_env(cli_env)
                .partitioner(partitioner.as_ref().map(|x| format!("{:?}", x)))
                .build()
                .unwrap();

            // arguments parsed, launch generation of ninja file(s)
            let builds = generator.execute(partitioner)?;

            let ninja_build_file = get_ninja_build_file(build_dir, &mode);

            if build_matches.get_flag("compile-commands") {
                let mut compile_commands = project_root.clone();
                compile_commands.push("compile_commands.json");
                println!("laze: generating {compile_commands}");
                ninja::generate_compile_commands(&ninja_build_file, &compile_commands)?;
            }

            // collect (optional) task and it's arguments
            let task = collect_tasks(build_matches);

            // generation of ninja build file complete.
            // exit here if requested.
            if task.is_none() && build_matches.get_flag("generate-only") {
                return Ok(0);
            }

            if let Some((task, args)) = task {
                let builds: Vec<&(String, String, BuildInfo)> = builds
                    .build_infos
                    .iter()
                    .filter(|(builder, app, build_info)| {
                        builders.selects(builder)
                            && apps.selects(app)
                            && build_info.tasks.contains_key(task)
                    })
                    .collect();

                if builds.len() > 1 {
                    eprintln!("laze: multiple task targets found:");
                    for (builder, bin, _build_info) in builds {
                        eprintln!("{} {}", builder, bin);
                    }

                    // TODO: allow running tasks for multiple targets
                    return Err(anyhow!("please specify one of these."));
                }

                if builds.is_empty() {
                    return Err(anyhow!("no matching target for task \"{}\" found.", task));
                }

                let build = builds[0];
                let targets = Some(vec![build.2.out.clone()]);

                let task_name = task;
                let task = build.2.tasks.get(task).unwrap();

                if task.build_app() && !build_matches.get_flag("generate-only") {
                    let ninja_build_file = get_ninja_build_file(build_dir, &mode);
                    if ninja_run(ninja_build_file.as_path(), verbose > 0, targets, jobs)? != 0 {
                        return Err(anyhow!("build error"));
                    };
                }

                println!(
                    "laze: executing task {} for builder {} bin {}",
                    task_name, build.0, build.1,
                );

                task.execute(project_root.as_ref(), args, verbose)?;
            } else {
                // build ninja target arguments, if necessary
                let targets: Option<Vec<Utf8PathBuf>> = if let Selector::All = builders {
                    if let Selector::All = apps {
                        None
                    } else {
                        // TODO: filter by app
                        None
                    }
                } else {
                    Some(
                        builds
                            .build_infos
                            .iter()
                            .filter_map(|(builder, app, build_info)| {
                                if builders.selects(builder) && apps.selects(app) {
                                    Some(build_info.out.clone())
                                } else {
                                    None
                                }
                            })
                            .collect(),
                    )
                };

                ninja_run(ninja_build_file.as_path(), verbose > 0, targets, jobs)?;
            }
        }
        Some(("clean", clean_matches)) => {
            let verbose = clean_matches.get_count("verbose");
            let unused = clean_matches.get_flag("unused");
            let build_dir = clean_matches.get_one::<Utf8PathBuf>("build-dir").unwrap();
            let mode = match global {
                true => GenerateMode::Global,
                false => GenerateMode::Local(start_relpath),
            };
            let ninja_build_file = get_ninja_build_file(build_dir, &mode);
            let tool = match unused {
                true => "cleandead",
                false => "clean",
            };
            let clean_target: Option<Vec<Utf8PathBuf>> = Some(vec!["-t".into(), tool.into()]);
            ninja_run(ninja_build_file.as_path(), verbose > 0, clean_target, None)?;
        }
        _ => {}
    };

    Ok(0)
}

fn collect_tasks(task_matches: &clap::ArgMatches) -> Option<(&str, Option<Vec<&str>>)> {
    match task_matches.subcommand() {
        Some((name, matches)) => {
            let args = matches
                .get_many::<std::ffi::OsString>("")
                .into_iter()
                .flatten()
                .map(|v| str::from_utf8(v.as_bytes()).expect("task arg is invalid UTF8"))
                .collect::<Vec<_>>();
            Some((name, Some(args)))
        }
        _ => None,
    }
}

fn get_cli_vars(
    build_matches: &clap::ArgMatches,
) -> Result<Option<im::HashMap<String, nested_env::EnvKey>>, Error> {
    let cli_env = if let Some(entries) = build_matches.get_many::<String>("define") {
        let mut env = Env::new();

        for assignment in entries {
            env = nested_env::assign_from_string(env, assignment)?;
        }

        Some(env)
    } else {
        None
    };
    Ok(cli_env)
}

fn get_disables(build_matches: &clap::ArgMatches) -> Option<Vec<String>> {
    let disable = build_matches
        .get_many::<String>("disable")
        .map(|vr| vr.cloned().collect_vec());
    disable
}

fn get_selects(build_matches: &clap::ArgMatches) -> Option<Vec<Dependency<String>>> {
    let select = build_matches.get_many::<String>("select");
    // convert CLI --select strings to Vec<Dependency>
    select.map(|vr| vr.map(crate::data::dependency_from_string).collect_vec())
}

#[cfg(test)]
mod test {
    #[test]
    fn test_clap() {
        crate::cli::clap().debug_assert();
    }
}
