use super::*;

pub(crate) fn run_frontend_dropin(root: PathBuf, raw_args: &[String], requested_runner: &str) -> Result<i32> {
    let root = root.canonicalize()?;
    let mut targets = Vec::new();
    let mut k: Option<String> = None;
    let mut no_cache = false;
    let mut extra_args = Vec::new();

    let mut i = 0usize;
    if matches!(raw_args.first().map(String::as_str), Some("run" | "watch")) {
        i = 1;
    }
    while i < raw_args.len() {
        let arg = &raw_args[i];
        match arg.as_str() {
            "-t" | "--testNamePattern" | "-k" => {
                if i + 1 < raw_args.len() && !raw_args[i + 1].starts_with('-') {
                    k = Some(raw_args[i + 1].clone());
                    i += 2;
                    continue;
                }
                eprintln!("error: option {arg} requires a value");
                return Ok(1);
            }
            "--no-cache" | "--clearCache" => {
                no_cache = true;
            }
            "--runTestsByPath" | "--watch=false" => {}
            // Two-token Vitest/Jest CLI options whose value must NOT be mistaken for a test target!
            "-c" | "--config" | "--root" | "--dir" | "--maxWorkers" | "-w"
            | "--maxConcurrency" | "--pool" | "--reporter" | "--reporters"
            | "--testPathPattern" | "--testPathIgnorePatterns" | "--setupFilesAfterEnv"
            | "--environment" | "--testEnvironment" | "--project" | "--shard"
            | "--outputFile" | "--coverageDirectory" | "--mode" | "--bail" | "-b"
            | "--testTimeout" | "--hookTimeout" | "--retry" => {
                extra_args.push(arg.clone());
                if i + 1 < raw_args.len() {
                    extra_args.push(raw_args[i + 1].clone());
                    i += 2;
                    continue;
                }
            }
            _ if arg.starts_with('-') => {
                extra_args.push(arg.clone());
            }
            _ => {
                // Vitest and Jest treat positional arguments like `Button` as filename substring
                // patterns rather than exact paths. Only route existing paths or `file::node`
                // selectors into `targets`; forward filename patterns directly to the runner!
                let file_part = arg.split("::").next().unwrap_or(arg);
                let exists_in_root = root.join(file_part).exists()
                    || Path::new(file_part).exists()
                    || arg.contains("::");
                if exists_in_root {
                    targets.push(arg.clone());
                } else {
                    extra_args.push(arg.clone());
                }
            }
        }
        i += 1;
    }

    let res = daemon::request_or_local_cli(&daemon::Request {
        root: Some(root),
        name: "test_run".to_string(),
        arguments: json!({
            "targets": targets,
            "k": k,
            "no_cache": no_cache,
            "rerun_failed": true,
            "full_suite": true,
            "language_filter": requested_runner,
            "pytest_args": extra_args,
        }),
        compact: false,
    })?;

    if let Some(summary) = res.get("stdout_summary").and_then(Value::as_str) {
        println!("{summary}");
    } else {
        print_json(&res)?;
    }

    match res["status"].as_str() {
        Some("failed" | "no_tests") => Ok(1),
        _ => Ok(0),
    }
}

