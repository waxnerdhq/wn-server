//! Checks `README.md` against the code: the configuration table holds every
//! key of `config.toml` with its default, and the command table holds every
//! command with its options.

use clap::{Command, CommandFactory};
use wn_server::cli::Cli;
use wn_server::config::KEYS;

/// The text of `README.md`.
const README: &str = include_str!("../README.md");

/// The lines of the section of `README.md` under the heading `## <title>`,
/// up to the next heading of that level.
fn section(title: &str) -> Vec<&'static str> {
    let heading = format!("## {title}");
    let lines: Vec<&str> = README
        .lines()
        .skip_while(|line| *line != heading)
        .skip(1)
        .take_while(|line| !line.starts_with("## "))
        .collect();
    assert!(!lines.is_empty(), "README.md has no section `{heading}`");
    lines
}

/// The table rows of `lines` whose first cell is code, as in
/// `` | `server.port` | `4200` | the port | ``. Each row is the text between
/// the backticks of the first cell, and the rest of the line after that
/// cell.
fn code_rows(lines: &[&'static str]) -> Vec<(&'static str, &'static str)> {
    lines
        .iter()
        .filter_map(|line| line.strip_prefix("| `")?.split_once("` |"))
        .collect()
}

/// The text between the backticks of the first cell of `rest`, which is the
/// rest of a table row.
fn code_cell(rest: &'static str) -> Option<&'static str> {
    let (cell, _) = rest.trim_start().strip_prefix('`')?.split_once('`')?;
    Some(cell)
}

/// Adds every command of `command` that has no subcommand to `commands`:
/// its words, as in `wn-server user show`, and the long names of its own
/// options.
fn collect_commands(command: &Command, words: &str, commands: &mut Vec<(String, Vec<String>)>) {
    let mut subcommands = command.get_subcommands().peekable();
    if subcommands.peek().is_none() {
        commands.push((words.to_owned(), own_options(command)));
        return;
    }
    for subcommand in subcommands {
        let words = format!("{words} {}", subcommand.get_name());
        collect_commands(subcommand, &words, commands);
    }
}

/// The long names of the options that `command` defines itself. An option
/// that every command accepts is not one of them.
fn own_options(command: &Command) -> Vec<String> {
    command
        .get_arguments()
        .filter(|argument| !argument.is_global_set())
        .filter_map(|argument| argument.get_long())
        .map(str::to_owned)
        .collect()
}

/// Whether the first cell of a command row, `cell`, is the row of the
/// command with these `words`.
fn is_row_of(cell: &str, words: &str) -> bool {
    cell.strip_prefix(words)
        .is_some_and(|rest| rest.is_empty() || rest.starts_with(' '))
}

#[test]
fn the_configuration_section_lists_every_key_with_its_default() {
    let documented: Vec<(&str, &str)> = code_rows(&section("Configuration"))
        .into_iter()
        .map(|(key, rest)| {
            let default = code_cell(rest)
                .unwrap_or_else(|| panic!("the row of `{key}` has no default in code form"));
            (key, default)
        })
        .collect();
    let keys: Vec<(&str, &str)> = KEYS.iter().map(|key| (key.name, key.default)).collect();
    // The same keys with the same defaults, in the order of `config.toml`.
    // A key of `config::KEYS` without a row fails here, and so does a row
    // whose key the server does not have.
    assert_eq!(documented, keys);
}

#[test]
fn the_commands_section_lists_every_command_with_its_options() {
    let lines = section("Commands");
    let root = Cli::command();
    // The rows of the command table start with the name of the program. The
    // section has other tables, such as the one of the environment
    // variables.
    let rows: Vec<(&str, &str)> = code_rows(&lines)
        .into_iter()
        .filter(|(cell, _)| is_row_of(cell, root.get_name()))
        .collect();
    let mut commands = Vec::new();
    collect_commands(&root, root.get_name(), &mut commands);
    assert!(
        commands.len() >= 7,
        "clap reports the commands {commands:?}"
    );

    for (words, options) in &commands {
        let (cell, _) = rows
            .iter()
            .find(|(cell, _)| is_row_of(cell, words))
            .unwrap_or_else(|| panic!("README.md has no row for `{words}`"));
        for option in options {
            assert!(
                cell.contains(&format!("--{option}")),
                "the row of `{words}` does not name --{option}: {cell}"
            );
        }
    }

    // An option that every command accepts is named in the section with the
    // name of its value, as in `--data-dir DIR`.
    let text = lines.join("\n");
    for argument in root.get_arguments().filter(|a| a.is_global_set()) {
        let long = argument.get_long().expect("a long option");
        let value = argument
            .get_value_names()
            .and_then(|names| names.first())
            .expect("the name of the value");
        let option = format!("`--{long} {value}`");
        assert!(text.contains(&option), "the section does not name {option}");
    }

    // A row names a command that exists, or the version option.
    assert!(root.get_version().is_some());
    let version = format!("{} --version", root.get_name());
    assert!(
        rows.iter().any(|(cell, _)| *cell == version),
        "README.md has no row for `{version}`"
    );
    for (cell, _) in &rows {
        assert!(
            *cell == version || commands.iter().any(|(words, _)| is_row_of(cell, words)),
            "README.md has a row for `{cell}`, which is no command"
        );
    }
}
