mod agents;
mod commands;
mod error;
mod hosts;
mod models;
mod paths;
mod safe_path;
mod services;
#[cfg(test)]
mod smoke;

use paths::ClaudePaths;

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    // Indexing and deep search are throughput work, not latency-critical UI work. Rayon defaults
    // to every logical CPU, which can starve the WebView even though the work is off the UI thread.
    // Reserve at least one logical CPU for UI/event processing and cap background fan-out.
    let logical_cpus = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(2);
    let background_threads = logical_cpus.saturating_sub(1).clamp(1, 4);
    let _ = rayon::ThreadPoolBuilder::new()
        .num_threads(background_threads)
        .thread_name(|index| format!("dejavu-cpu-{index}"))
        .build_global();

    let claude_paths = ClaudePaths::new();

    let multi_hosts = agents::default_providers(claude_paths.clone());
    let providers: Vec<std::sync::Arc<dyn agents::AgentProvider>> = multi_hosts
        .iter()
        .map(|provider| provider.clone() as std::sync::Arc<dyn agents::AgentProvider>)
        .collect();
    let registry = agents::ProviderRegistry::new(providers.clone());

    // Global search index: every provider contributes its own searchable documents.
    let search_engine = services::search::build_in_background(providers.clone());
    // Keep the index fresh when sessions change on disk (no app restart needed).
    services::search::spawn_auto_refresh(search_engine.clone(), providers.clone());
    // Agent installs inside WSL join the same sources once found — off the startup path, since
    // reading a distro's share boots it.
    spawn_wsl_discovery(multi_hosts, providers, search_engine.clone());

    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_process::init())
        .plugin(tauri_plugin_updater::Builder::new().build())
        .manage(claude_paths)
        .manage(search_engine)
        .manage(registry)
        .invoke_handler(tauri::generate_handler![
            commands::sessions::list_sources,
            commands::instructions::list_instruction_artifacts,
            commands::instructions::get_instruction_artifact,
            commands::instructions::save_instruction_artifact,
            commands::instructions::get_project_context,
            commands::profiles::list_profiles,
            commands::profiles::create_profile,
            commands::profiles::restore_profile,
            commands::profiles::delete_profile,
            commands::profiles::rename_profile,
            commands::memories::list_projects,
            commands::memories::list_memories,
            commands::memories::get_memory,
            commands::memories::save_memory,
            commands::memories::delete_memory,
            commands::memories::create_memory,
            commands::rules::list_rules,
            commands::rules::get_rule,
            commands::rules::toggle_rule,
            commands::sessions::list_sessions,
            commands::sessions::search_sessions,
            commands::sessions::deep_search,
            commands::sessions::get_index_status,
            commands::sessions::rebuild_index,
            commands::sessions::usage_summary,
            commands::sessions::dashboard_summary,
            commands::sessions::browse_sessions,
            commands::sessions::get_session_detail,
            commands::sessions::get_session_tail,
            commands::sessions::get_session_before,
            commands::sessions::get_session_first_prompt,
            commands::sessions::list_subagents,
            commands::sessions::get_subagent_detail,
            commands::sessions::search_in_session,
            commands::session_meta::list_session_meta,
            commands::session_meta::set_session_meta,
            commands::workflows::list_workflows,
            commands::workflows::read_workflow,
            commands::tools::list_tools,
            commands::shell::resume_session,
            commands::shell::open_in_terminal,
            commands::shell::open_external,
            commands::shell::save_text_export,
            commands::shell::reveal_path,
            commands::shell::get_dejavu_config,
            commands::shell::save_dejavu_config,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}

/// Find agent installs inside WSL and fold them into the sources that already exist.
///
/// This runs on its own thread because the first read of `\\wsl.localhost\<distro>` starts that
/// distro — acceptable when it happens behind an already-usable window, not while one is opening.
/// Once hosts are adopted the index is rebuilt, so their sessions appear without a restart; the
/// retry loop is there because the initial index build holds the indexing slot for a while.
fn spawn_wsl_discovery(
    multi_hosts: Vec<std::sync::Arc<agents::MultiHostProvider>>,
    providers: Vec<std::sync::Arc<dyn agents::AgentProvider>>,
    engine: services::search::SharedSearchEngine,
) {
    std::thread::spawn(move || {
        let config = commands::shell::load_config();
        if !config.wsl_scan {
            return;
        }
        let homes = hosts::discover_wsl_homes(&config.wsl_excluded);
        if homes.is_empty() {
            return;
        }
        hosts::publish_wsl_homes(&homes);
        for provider in &multi_hosts {
            provider.adopt(&homes);
        }
        for _ in 0..60 {
            if services::search::rebuild(&engine, &providers) {
                return;
            }
            std::thread::sleep(std::time::Duration::from_secs(2));
        }
    });
}
