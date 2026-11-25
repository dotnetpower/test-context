use super::*;

pub(crate) fn run_pytest_dropin(root: PathBuf, raw_args: &[String]) -> Result<i32> {
    let root = root.canonicalize()?;
    let mut targets = Vec::new();
    let mut k: Option<String> = None;
    let mut mark: Option<String> = None;
    let mut no_cache = false;
    let mut extra_args = Vec::new();

    let mut i = 0usize;
    while i < raw_args.len() {
        let arg = &raw_args[i];
        match arg.as_str() {
            "-k" => {
                if i + 1 < raw_args.len() && !raw_args[i + 1].starts_with('-') {
                    k = Some(raw_args[i + 1].clone());
                    i += 2;
                    continue;
                }
                eprintln!("ERROR: argument -k: expected one argument");
                return Ok(4);
            }
            "-m" => {
                if i + 1 < raw_args.len() && !raw_args[i + 1].starts_with('-') {
                    mark = Some(raw_args[i + 1].clone());
                    i += 2;
                    continue;
                }
                eprintln!("ERROR: argument -m: expected one argument");
                return Ok(4);
            }
            "--cache-clear" | "--no-cache" => {
                no_cache = true;
            }
            "-q" | "--quiet" | "-v" | "--verbose" => {
                // Handled by tctx runner summary
            }
            "-c" | "-o" | "-p" | "-W" | "-n" | "--maxfail" | "--tb" | "--rootdir"
            | "--confcutdir" | "--import-mode" | "--basetemp" | "--durations"
            | "--durations-min" | "--cov" | "--cov-report" | "--cov-config" | "--log-level"
            | "--log-format" | "--log-cli-level" | "--color" | "--capture" => {
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
                targets.push(arg.clone());
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
            "mark": mark,
            "no_cache": no_cache,
            "rerun_failed": true,
            "full_suite": true,
            "language_filter": "python",
            "pytest_args": extra_args,
        }),
        compact: false,
    })?;

    if let Some(summary) = res.get("stdout_summary").and_then(Value::as_str) {
        println!("{summary}");
    } else {
        print_json(&res)?;
    }

    if let Some(code) = res
        .get("runner_exit_code")
        .and_then(Value::as_i64)
        .and_then(|c| i32::try_from(c).ok())
        && code != 0
    {
        return Ok(code);
    }

    match res["status"].as_str() {
        Some("failed") => Ok(1),
        Some("no_tests") => Ok(5),
        _ => Ok(0),
    }
}

