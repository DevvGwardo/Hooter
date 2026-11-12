//! Shared command evidence classification. This is not an approval allowlist
//! or a shell sandbox. Unknown effects remain conservative; check programs
//! may write build artifacts, but are not presumed to edit source files.

use crate::harness::guard::is_inspection;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct CommandEvidence {
    pub verification: bool,
    pub mutates: bool,
}

pub(crate) fn classify(command: &str) -> CommandEvidence {
    classify_with(command, cfg!(windows))
}

fn classify_with(command: &str, windows: bool) -> CommandEvidence {
    let Some(parts) = shell_parts_with(command, windows) else {
        return CommandEvidence {
            // A shell test still owns its final exit status when its operands
            // expand. Those expansions remain potentially mutating.
            verification: command.trim_start().starts_with("test ")
                && !command.contains(['|', '&', ';', '\n', '\r', '>', '<', '`']),
            mutates: true,
        };
    };
    let mut verification = false;
    let mut mutates = false;
    for (part, sequence) in parts {
        let Some(words) = split_words_with(part, windows) else {
            return CommandEvidence {
                verification: false,
                mutates: true,
            };
        };
        let words: Vec<_> = words.iter().map(String::as_str).collect();
        let words = executable_words(&words);
        let check = is_check(words);
        let part_mutates = !check_without_source_writes(words) && !inspection(words);
        if sequence || part_mutates {
            // A semicolon/newline doesn't propagate an earlier command's exit,
            // and a subsequent mutating or unknown-effect part invalidates any
            // earlier verification check.
            verification = check;
        } else {
            verification |= check;
        }
        mutates |= part_mutates;
    }
    CommandEvidence {
        verification,
        mutates,
    }
}

/// Advisory only. Never turns an unattributable shell status into evidence.
pub(crate) fn verification_rejection(command: &str) -> Option<&'static str> {
    if classify(command).verification {
        return None;
    }
    let attempted = command.split(['&', '|', ';', '\n']).any(|part| {
        split_words_with(part, cfg!(windows)).is_some_and(|words| {
            let words: Vec<_> = words.iter().map(String::as_str).collect();
            is_check(executable_words(&words))
        })
    });
    attempted.then_some(
        "The shell wrapper does not preserve a recognizable check's exit status. \
         Run the test/typecheck/build directly, without pipes, `; echo`, `|| true`, \
         backgrounding, or file redirection. Output is already bounded to its head and tail.",
    )
}

fn executable_words<'a>(mut words: &'a [&'a str]) -> &'a [&'a str] {
    if words.first() == Some(&"env") {
        words = &words[1..];
    }
    while words.first().is_some_and(|word| {
        word.split_once('=').is_some_and(|(name, _)| {
            !name.is_empty()
                && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
                && !name.starts_with(|c: char| c.is_ascii_digit())
        })
    }) {
        words = &words[1..];
    }
    words
}

/// A narrow compile-only failure, not a failed assertion. Do not infer this
/// from diagnostic prose alone: the inline source must be plainly invalid too.
pub(crate) fn is_unexecuted_python_check(command: &str, detail: &str) -> bool {
    if !detail.starts_with("  File \"<string>\", line ")
        || !detail.lines().any(|line| line.starts_with("SyntaxError:"))
    {
        return false;
    }
    let Some(parts) = shell_parts_with(command, cfg!(windows)) else {
        return false;
    };
    if parts.len() != 1 {
        return false;
    }
    let Some(words) = split_words_with(parts[0].0, cfg!(windows)) else {
        return false;
    };
    let word_refs: Vec<_> = words.iter().map(String::as_str).collect();
    let Some((first, rest)) = word_refs.split_first() else {
        return false;
    };
    let program = program_name(first);
    let Some(["-c", code]) = python_args(&program, rest) else {
        return false;
    };
    let code = code.as_bytes();
    let (mut quote, mut comment, mut invalid, mut i) = (None, false, false, 0);
    while i < code.len() {
        let c = code[i];
        if comment {
            comment = c != b'\n';
        } else if let Some(q) = quote {
            if c == b'\\' {
                i += 2;
                continue;
            }
            if c == q {
                quote = None;
            }
        } else if matches!(c, b'\'' | b'"') {
            // Prefixed/f/triple strings need a full Python parser. Stay
            // conservative instead of guessing at their embedded expressions.
            if (i > 0 && (code[i - 1].is_ascii_alphanumeric() || code[i - 1] == b'_'))
                || code.get(i..i + 3).is_some_and(|s| s == [c, c, c])
            {
                return false;
            }
            quote = Some(c);
        } else if c == b'#' {
            comment = true;
        } else if c == b'\\'
            && code
                .get(i + 1)
                .is_some_and(|c| matches!(c, b'n' | b'r' | b't'))
        {
            invalid = true;
        } else if c == b';' {
            let mut next = i + 1;
            while code.get(next).is_some_and(|c| matches!(c, b' ' | b'\t')) {
                next += 1;
            }
            // A compound try statement cannot follow a semicolon on the
            // same logical line. A real newline remains valid.
            invalid |= code.get(next..next + 4) == Some(b"try:");
        }
        i += 1;
    }
    invalid && quote.is_none()
}

fn program_name(program: &str) -> String {
    let base = program.rsplit(['/', '\\']).next().unwrap_or(program);
    let lower = base.to_ascii_lowercase();
    for ext in [".exe", ".cmd", ".bat"] {
        if let Some(stripped) = lower.strip_suffix(ext) {
            return stripped.to_string();
        }
    }
    lower
}

fn is_py_launcher_version_flag(flag: &str) -> bool {
    let Some(rest) = flag.strip_prefix('-') else {
        return false;
    };
    !rest.is_empty()
        && rest.starts_with(|c: char| c.is_ascii_digit())
        && rest
            .chars()
            .all(|c| c.is_ascii_digit() || matches!(c, '.' | '-'))
}

fn python_args<'a>(program: &str, args: &'a [&'a str]) -> Option<&'a [&'a str]> {
    match program {
        "python" | "python3" => Some(args),
        "py" => {
            if let Some((first, rest)) = args.split_first()
                && is_py_launcher_version_flag(first)
            {
                Some(rest)
            } else {
                Some(args)
            }
        }
        _ => None,
    }
}

fn has_mutating_check_flag(words: &[&str]) -> bool {
    words.iter().any(|word| {
        matches!(*word, "--fix" | "--fix-only" | "--write" | "-w")
            || word.starts_with("--fix=")
            || word.starts_with("--fix-only=")
            || word.starts_with("--write=")
    })
}

fn is_check(words: &[&str]) -> bool {
    if has_mutating_check_flag(words) {
        return false;
    }
    let Some((program, args)) = words.split_first() else {
        return false;
    };
    let program = program_name(program);
    if let Some(py_args) = python_args(&program, args) {
        return match py_args {
            ["-m", "pytest" | "unittest" | "py_compile", ..] => true,
            ["-c", code, ..] => code.contains("assert "),
            [script, ..] => std::path::Path::new(script)
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("test_") || name.starts_with("validate")),
            _ => false,
        };
    }
    match (program.as_str(), args) {
        ("test" | "pytest" | "pytest-3" | "jest" | "vitest" | "mypy" | "eslint", _) => true,
        ("cargo", ["test" | "check" | "clippy" | "build", ..]) => true,
        ("cargo", ["fmt", rest @ ..]) => rest.contains(&"--check"),
        ("rustfmt" | "prettier", rest) => rest.contains(&"--check"),
        ("tsc", rest) => rest.contains(&"--noEmit"),
        ("npm" | "pnpm" | "yarn", ["exec", rest @ ..]) | ("npx" | "bunx", rest) => {
            let start = rest
                .iter()
                .position(|arg| !matches!(*arg, "--" | "-y" | "--yes" | "--no-install"))
                .unwrap_or(rest.len());
            is_check(&rest[start..])
        }
        (
            "npm" | "pnpm" | "yarn" | "bun",
            ["test" | "check" | "lint" | "typecheck" | "build", ..],
        )
        | (
            "npm" | "pnpm" | "yarn" | "bun",
            [
                "run",
                "test" | "check" | "lint" | "typecheck" | "build" | "validate",
                ..,
            ],
        ) => true,
        ("go", ["test" | "vet" | "build", ..]) => true,
        ("git", rest) => {
            crate::harness::guard::git_subcommand(rest) == Some("diff") && rest.contains(&"--check")
        }
        ("ruff", ["check", ..]) | ("biome", ["check" | "lint", ..]) => true,
        ("ruff", ["format", rest @ ..]) => rest.contains(&"--check"),
        _ => false,
    }
}

fn check_without_source_writes(words: &[&str]) -> bool {
    let inline_python = words.split_first().is_some_and(|(program, args)| {
        let program = program_name(program);
        python_args(&program, args).is_some_and(|py_args| matches!(py_args, ["-c", ..]))
    });
    is_check(words)
        && !words.iter().any(|word| word.starts_with("--output"))
        // Inline assertions can also contain arbitrary mutations.
        && !inline_python
}

fn inspection(words: &[&str]) -> bool {
    !words.iter().any(|word| {
        matches!(
            *word,
            "-i" | "--in-place" | "-delete" | "-exec" | "-execdir" | "-fprint"
        ) || word.starts_with("--output")
            || word.starts_with("--pre")
            || word.starts_with("--open-files-in-pager")
            || word.starts_with("--ext-diff")
            || word.starts_with("--textconv")
    }) && words.first().is_some_and(|program| {
        let program = program_name(program);
        match program.as_str() {
            "cargo" | "npm" | "pnpm" | "yarn" | "bun" | "go" | "rustc" | "rustfmt"
                if matches!(&words[1..], ["--version" | "-V" | "--help" | "-h"]) =>
            {
                true
            }
            "git" => crate::harness::guard::git_subcommand(&words[1..]).is_some_and(|sub| {
                matches!(
                    sub,
                    "status" | "diff" | "log" | "show" | "branch" | "blame" | "ls-files"
                )
            }),
            // An awk program can invoke a command or write a file.
            "awk" => false,
            _ => is_inspection(&program),
        }
    })
}

fn has_windows_percent_expansion(command: &str) -> bool {
    let bytes = command.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let next = bytes.get(i + 1).copied();
            if matches!(next, Some(b'0'..=b'9' | b'*' | b'~')) {
                return true;
            }
            // Reject paired %VAR% (no whitespace between the pair).
            let mut j = i + 1;
            while j < bytes.len() && !bytes[j].is_ascii_whitespace() {
                if bytes[j] == b'%' && j > i + 1 {
                    return true;
                }
                j += 1;
            }
        }
        i += 1;
    }
    false
}

fn has_windows_bang_expansion(command: &str) -> bool {
    // cmd delayed expansion (`!VAR!`) uses paired bangs. Single `!` or `!=`
    // operators cannot expand environment variables.
    let non_operator_bangs = command
        .as_bytes()
        .windows(2)
        .filter(|pair| pair[0] == b'!' && pair[1] != b'=')
        .count()
        + usize::from(command.ends_with('!') && !command.ends_with("!="));
    non_operator_bangs >= 2
}

fn split_words_with(part: &str, windows: bool) -> Option<Vec<String>> {
    if !windows {
        return shlex::split(part);
    }
    let mut words = Vec::new();
    let mut current = String::new();
    let mut in_word = false;
    let mut quote = None;
    for c in part.chars() {
        if let Some(q) = quote {
            if c == q {
                quote = None;
            } else {
                current.push(c);
            }
            continue;
        }
        match c {
            '\'' | '"' => {
                quote = Some(c);
                in_word = true;
            }
            c if c.is_ascii_whitespace() => {
                if in_word {
                    words.push(std::mem::take(&mut current));
                    in_word = false;
                }
            }
            _ => {
                current.push(c);
                in_word = true;
            }
        }
    }
    if quote.is_some() {
        return None;
    }
    if in_word {
        words.push(current);
    }
    Some(words)
}

/// A deliberately small foreground grammar. Expansions, redirection,
/// conditionals and pipelines cannot reliably attribute a check's exit code.
/// Quoted punctuation is data, not another command.
fn shell_parts_with(command: &str, windows: bool) -> Option<Vec<(&str, bool)>> {
    if windows && (has_windows_percent_expansion(command) || has_windows_bang_expansion(command)) {
        return None;
    }
    let mut quote = None;
    let mut escaped = false;
    let mut start = 0;
    let mut sequence = false;
    let mut parts = Vec::new();
    let mut chars = command.char_indices().peekable();
    while let Some((index, c)) = chars.next() {
        if escaped {
            escaped = false;
            continue;
        }
        if !windows && c == '\\' && quote != Some('\'') {
            escaped = true;
            continue;
        }
        if quote == Some('\'') {
            if c == '\'' {
                quote = None;
            }
            continue;
        }
        if matches!(c, '$' | '`') {
            return None;
        }
        if windows && c == '^' && quote != Some('"') {
            return None;
        }
        if let Some(q) = quote {
            if c == q {
                quote = None;
            }
            continue;
        }
        // Descriptor merging does not write a file or mask the exit status.
        // Accept this exact standalone spelling, not arbitrary redirections.
        if c == '2'
            && command[index..].starts_with("2>&1")
            && (index == 0 || command[..index].ends_with(char::is_whitespace))
            && command[index + 4..]
                .chars()
                .next()
                .is_none_or(|c| c.is_whitespace() || matches!(c, '&' | ';'))
        {
            for _ in 0..3 {
                chars.next();
            }
            continue;
        }
        match c {
            '\'' | '"' => quote = Some(c),
            '|' | '<' | '>' | '(' | ')' | '#' | '\r' => return None,
            '&' | ';' | '\n' => {
                let and = c == '&';
                if and && chars.next().map(|(_, c)| c) != Some('&') {
                    return None;
                }
                let part = command[start..index].trim();
                if !part.is_empty() {
                    parts.push((part, sequence));
                    sequence = !and;
                } else if and || c == ';' {
                    return None;
                }
                start = chars.peek().map_or(command.len(), |(i, _)| *i);
            }
            _ => {}
        }
    }
    if quote.is_some() || escaped {
        return None;
    }
    let last = command[start..].trim();
    if !last.is_empty() {
        parts.push((last, sequence));
    } else if !parts.is_empty() && !sequence {
        return None;
    }
    (!parts.is_empty()).then_some(parts)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unexecuted_python_checks_require_static_compile_errors_not_diagnostic_claims() {
        let detail = "  File \"<string>\", line 1\nSyntaxError: invalid syntax";
        for command in [
            r#"python3 -c "assert True\nassert True""#,
            r#"python3 -c "assert True; try: pass""#,
        ] {
            assert!(is_unexecuted_python_check(command, detail), "{command}");
            assert!(!is_unexecuted_python_check(
                command,
                "SyntaxError: invalid syntax"
            ));
            assert!(!is_unexecuted_python_check(
                command,
                "Traceback (most recent call last):\nSyntaxError: invalid syntax"
            ));
        }
        for command in [
            r#"python3 -c "assert '\\n'""#,
            r#"python3 -c "assert True # \n""#,
            r#"python3 -c "assert True; print('try:')""#,
            r#"python3 -c "assert f'\\n'""#,
            "python3 -c 'assert True;\ntry:\n pass\nexcept ValueError:\n pass'",
            r#"python3 -c "assert True\nassert True" && touch output"#,
            r#"python3 -c "assert False""#,
            "cargo test",
        ] {
            assert!(!is_unexecuted_python_check(command, detail), "{command}");
        }
    }

    #[test]
    fn common_checks_share_verification_and_effect_classification() {
        for command in [
            "cargo fmt --check",
            "cargo fmt --all -- --check",
            "npx tsc --noEmit",
            "npx eslint src",
            "ruff check src",
            "ruff format --check src",
            "prettier --check src",
            "env CI=1 cargo test",
            "cd 'two words' && cargo test && git status --short",
            "git diff --stat && npm run build",
            "cargo test &&\n git status --short",
            "npx --yes tsc --noEmit",
            "git diff --check",
            "bun test",
            "bun run typecheck",
            "bun run check",
            "bunx tsc --noEmit",
            "cd 'two words' && bun run typecheck 2>&1 && echo checked",
            "bun test 2>&1",
        ] {
            assert_eq!(
                classify(command),
                CommandEvidence {
                    verification: true,
                    mutates: false,
                },
                "{command}"
            );
        }
    }

    #[test]
    fn effects_and_exit_status_are_never_guessed_from_check_named_operands() {
        for command in [
            "touch test_output",
            "printf test | sh",
            "cargo test $(touch result)",
            "cargo test > result",
            "cargo test &",
            "true || cargo test",
            "git diff --output=result",
            "find . -delete",
            "cargo fmt",
            "npx eslint --fix src",
            "bun test | tail -20",
            "bun test || true",
            "bun test 2> errors.txt",
            "bun run lint --fix",
        ] {
            assert!(classify(command).mutates, "{command}");
        }
        assert!(!classify("touch test_output").verification);
        assert!(!classify("cargo test; echo done").verification);
        assert!(!classify("bun run typecheck; echo done").verification);
        assert!(!classify("cargo test | tail").verification);
        assert!(classify("touch output; test -s output").verification);
        assert!(!classify("echo 'cargo test; touch x'").verification);
        assert!(!classify("echo 'cargo test; touch x'").mutates);
        assert!(verification_rejection("bun test | tail -20").is_some());
        assert!(verification_rejection("bun run typecheck; echo done").is_some());
        assert!(verification_rejection("bun test").is_none());
        assert!(!classify("bun run arbitrary").verification);
    }

    #[test]
    fn mutating_parts_after_check_and_fix_flags_invalidate_verification() {
        for command in [
            "cargo test && touch tampered",
            "cargo test && unknown-helper --arg",
            "cargo test && ruff check --fix-only src",
            "ruff check --fix-only src",
            "ruff check --fix src",
            "npx eslint --fix src",
            "prettier --write src",
            "prettier -w src",
            "cargo clippy --fix --allow-dirty",
            "biome check --write src",
            "bun run lint --fix",
        ] {
            assert_eq!(
                classify(command),
                CommandEvidence {
                    verification: false,
                    mutates: true,
                },
                "{command}"
            );
        }
    }

    #[test]
    fn windows_paths_extensions_and_py_launcher_classify_cleanly() {
        for command in [
            r"C:\Python312\python.exe -m pytest",
            r#""C:\Program Files\Python312\PY.EXE" -3 -m unittest"#,
            "py -m pytest",
            "py -3.11 -m py_compile app.py",
            r".\node_modules\.bin\tsc.cmd --noEmit",
            r".\node_modules\.bin\ESLINT.CMD src",
            "CARGO.EXE test",
            r"C:\tools\pytest.bat -k test_ok",
        ] {
            assert_eq!(
                classify_with(command, true),
                CommandEvidence {
                    verification: true,
                    mutates: false,
                },
                "{command}"
            );
        }
        assert_eq!(
            classify_with(r#"py -c "assert 5 % 2 == 1 and 1 != 2""#, true),
            CommandEvidence {
                verification: true,
                mutates: true,
            }
        );
        assert_eq!(
            classify_with(r#"C:\Python312\python.exe test_app.py"#, true),
            CommandEvidence {
                verification: true,
                mutates: false,
            }
        );
        for unsafe_win in [
            "cargo test && touch tampered",
            "git status %INJECT%",
            "git status !INJECT!",
            "git status ^& echo changed",
            r"C:\tools\ruff.exe check --fix-only src",
        ] {
            assert_eq!(
                classify_with(unsafe_win, true),
                CommandEvidence {
                    verification: false,
                    mutates: true,
                },
                "{unsafe_win}"
            );
        }
    }
}
