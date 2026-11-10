use std::process::Command;

#[test]
fn suite_help_names_the_three_suite_default_and_the_opt_in_suite() {
    for command in ["collect", "collect-commit", "collect-base", "verify"] {
        let output = Command::new(env!("CARGO_BIN_EXE_cubrid-ci"))
            .args([command, "--help"])
            .output()
            .unwrap();
        assert!(output.status.success(), "{command} --help failed");
        let help = String::from_utf8(output.stdout).unwrap();
        let help = help.split_whitespace().collect::<Vec<_>>().join(" ");
        assert!(
            help.contains(
                "default: test_medium, test_sql, test_shell; test_sql_by_cci only when named"
            ),
            "{command} --help misstates the default suites:\n{help}"
        );
        assert!(
            !help.contains("by default)"),
            "{command} --help still claims all suites"
        );
    }
}
