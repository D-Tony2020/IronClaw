//! `ironclaw build` CLI subcommand.
//!
//! LLM-driven software building from natural language descriptions.
//! Supports WASM tools, CLI binaries, scripts, and libraries.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use clap::Subcommand;

use crate::bootstrap::ironclaw_base_dir;
use crate::config::Config;
use crate::llm::{SessionConfig, SessionManager};
use crate::safety::SafetyLayer;
use crate::tools::builder::{BuilderConfig, Language, LlmSoftwareBuilder, SoftwareBuilder, SoftwareType};
use crate::tools::ToolRegistry;

#[derive(Subcommand, Debug, Clone)]
pub enum BuildCommand {
    /// Build new software from a natural language description
    New {
        /// Natural language description of what to build
        description: String,

        /// Software type: wasm-tool, cli, library, script, web-service
        #[arg(short, long, default_value = "wasm-tool")]
        r#type: String,

        /// Language: rust, python, typescript, javascript, go, bash
        #[arg(short, long, default_value = "rust")]
        language: String,

        /// Output directory for built artifacts
        #[arg(short, long)]
        output: Option<PathBuf>,

        /// Maximum build loop iterations
        #[arg(long, default_value = "10")]
        max_iterations: u32,

        /// Build timeout in seconds
        #[arg(long, default_value = "600")]
        timeout: u64,

        /// Auto-register WASM tools after successful build
        #[arg(long, default_value = "true")]
        auto_register: bool,
    },
}

/// Parse software type from CLI string.
fn parse_software_type(s: &str) -> SoftwareType {
    match s.to_lowercase().as_str() {
        "wasm-tool" | "wasm_tool" | "wasm" | "tool" => SoftwareType::WasmTool,
        "cli" | "cli-binary" | "binary" => SoftwareType::CliBinary,
        "library" | "lib" | "crate" => SoftwareType::Library,
        "script" => SoftwareType::Script,
        "web-service" | "web_service" | "service" | "api" => SoftwareType::WebService,
        _ => {
            eprintln!("Warning: Unknown software type '{}', defaulting to wasm-tool", s);
            SoftwareType::WasmTool
        }
    }
}

/// Parse language from CLI string.
fn parse_language(s: &str) -> Language {
    match s.to_lowercase().as_str() {
        "rust" | "rs" => Language::Rust,
        "python" | "py" => Language::Python,
        "typescript" | "ts" => Language::TypeScript,
        "javascript" | "js" => Language::JavaScript,
        "go" | "golang" => Language::Go,
        "bash" | "sh" | "shell" => Language::Bash,
        _ => {
            eprintln!("Warning: Unknown language '{}', defaulting to rust", s);
            Language::Rust
        }
    }
}

/// Run a build command.
pub async fn run_build_command(cmd: BuildCommand) -> anyhow::Result<()> {
    match cmd {
        BuildCommand::New {
            description,
            r#type,
            language,
            output,
            max_iterations,
            timeout,
            auto_register,
        } => {
            run_build_new(
                description,
                parse_software_type(&r#type),
                parse_language(&language),
                output,
                max_iterations,
                timeout,
                auto_register,
            )
            .await
        }
    }
}

/// Execute a new build from a description.
async fn run_build_new(
    description: String,
    software_type: SoftwareType,
    language: Language,
    output: Option<PathBuf>,
    max_iterations: u32,
    timeout: u64,
    auto_register: bool,
) -> anyhow::Result<()> {
    eprintln!("🔧 IronClaw Builder");
    eprintln!("   Description: {}", description);
    eprintln!("   Type: {:?}, Language: {:?}", software_type, language);
    eprintln!("   Max iterations: {}, Timeout: {}s", max_iterations, timeout);
    eprintln!();

    // 1. Load config
    let config = Config::from_env().await.map_err(|e| anyhow::anyhow!("{}", e))?;

    // 2. Create session manager
    let session = Arc::new(SessionManager::new(SessionConfig::default()));

    // 3. Create LLM provider
    let (llm, _cheap_llm, _recording_handle) =
        crate::llm::build_provider_chain(&config.llm, session)?;
    eprintln!("   LLM: {}", llm.model_name());

    // 4. Create safety layer
    let safety = Arc::new(SafetyLayer::new(&config.safety));

    // 5. Create tool registry with builtin tools
    let tools = Arc::new(ToolRegistry::new());
    tools.register_builtin_tools();

    // 6. Configure builder
    let wasm_output_dir = output.or_else(|| {
        if matches!(software_type, SoftwareType::WasmTool) {
            Some(ironclaw_base_dir().join("tools"))
        } else {
            None
        }
    });

    let builder_config = BuilderConfig {
        build_dir: std::env::temp_dir().join("ironclaw-builds"),
        max_iterations,
        timeout: Duration::from_secs(timeout),
        cleanup_on_failure: false,
        validate_wasm: matches!(software_type, SoftwareType::WasmTool),
        run_tests: true,
        auto_register,
        wasm_output_dir,
    };

    let builder = LlmSoftwareBuilder::new(builder_config, llm, safety, tools);

    // 7. Analyze the requirement
    eprintln!("📋 Analyzing requirement...");
    let requirement = builder.analyze(&description).await.map_err(|e| {
        anyhow::anyhow!("Failed to analyze requirement: {}", e)
    })?;

    eprintln!("   Name: {}", requirement.name);
    eprintln!("   Type: {:?}", requirement.software_type);
    eprintln!("   Language: {:?}", requirement.language);
    if !requirement.dependencies.is_empty() {
        eprintln!("   Dependencies: {}", requirement.dependencies.join(", "));
    }
    eprintln!();

    // 8. Execute the build
    eprintln!("🏗️  Building...");
    let result = builder.build(&requirement).await.map_err(|e| {
        anyhow::anyhow!("Build failed: {}", e)
    })?;

    // 9. Report results
    eprintln!();
    if result.success {
        eprintln!("✅ Build succeeded!");
        eprintln!("   Artifact: {}", result.artifact_path.display());
        eprintln!("   Iterations: {}", result.iterations);
        if result.tests_passed > 0 {
            eprintln!(
                "   Tests: {} passed, {} failed",
                result.tests_passed, result.tests_failed
            );
        }
        if result.registered {
            eprintln!("   Registered: yes (available as agent tool)");
        }
        if !result.validation_warnings.is_empty() {
            eprintln!("   Warnings:");
            for w in &result.validation_warnings {
                eprintln!("     ⚠ {}", w);
            }
        }
        let duration = result
            .completed_at
            .signed_duration_since(result.started_at);
        eprintln!("   Duration: {}s", duration.num_seconds());
    } else {
        eprintln!("❌ Build failed!");
        if let Some(ref error) = result.error {
            eprintln!("   Error: {}", error);
        }
        eprintln!("   Iterations used: {}", result.iterations);
        eprintln!("   Build logs at: {}", result.artifact_path.display());

        // Show last few build log entries
        let recent_logs: Vec<_> = result.logs.iter().rev().take(5).collect();
        if !recent_logs.is_empty() {
            eprintln!();
            eprintln!("   Recent build log:");
            for log in recent_logs.iter().rev() {
                eprintln!("     [{:?}] {}", log.phase, log.message);
            }
        }
    }

    Ok(())
}
