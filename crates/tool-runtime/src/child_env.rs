//! Provider credentials belong to the model transport, not tool children.
//! This prevents accidental inheritance; it is not a same-user OS sandbox.

const PROVIDER_CREDENTIALS: &[&str] = &[
    "OPENAI_API_KEY",
    "DEEPSEEK_API_KEY",
    "ANTHROPIC_API_KEY",
    "AZURE_OPENAI_API_KEY",
    "AZURE_OPENAI_AD_TOKEN",
    "GEMINI_API_KEY",
    "GOOGLE_API_KEY",
];

pub(crate) fn remove_provider_credentials(command: &mut std::process::Command) {
    for key in PROVIDER_CREDENTIALS {
        command.env_remove(key);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn credential_child_fixture() {
        if std::env::var_os("CONTEXT_AGENT_ENV_FIXTURE").is_none() {
            return;
        }
        for key in PROVIDER_CREDENTIALS {
            assert!(
                std::env::var_os(key).is_none(),
                "credential inherited: {key}"
            );
        }
        assert_eq!(std::env::var("MYSQL_HOST").unwrap(), "task-database");
    }

    #[test]
    fn real_child_loses_provider_credentials_but_keeps_task_configuration() {
        let mut command = std::process::Command::new(std::env::current_exe().unwrap());
        command.args(["--exact", "child_env::tests::credential_child_fixture"]);
        command.env("CONTEXT_AGENT_ENV_FIXTURE", "1");
        command.env("MYSQL_HOST", "task-database");
        for key in PROVIDER_CREDENTIALS {
            command.env(key, "synthetic-test-credential");
        }
        remove_provider_credentials(&mut command);
        assert!(command.status().unwrap().success());
    }
}
