from pathlib import Path

path = Path('src/llama.rs')
text = path.read_text()

old = '''        if args.is_empty() || !args.iter().any(|arg| arg.contains("llama-server")) {
            continue;
        }
'''
new = '''        if args.is_empty() || !is_llama_server_process(&args) {
            continue;
        }
'''
if old not in text:
    raise SystemExit('process filter pattern not found')
text = text.replace(old, new, 1)

anchor = '''fn command_targets_port(args: &[String], target_port: u16) -> bool {
'''
helper = '''fn is_llama_server_process(args: &[String]) -> bool {
    let Some(executable) = args.first() else {
        return false;
    };
    let executable = Path::new(executable)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(executable);

    executable.contains("llama-server")
        || (executable == "llama" && args.get(1).is_some_and(|arg| arg == "serve"))
}

'''
if anchor not in text:
    raise SystemExit('command_targets_port anchor not found')
text = text.replace(anchor, helper + anchor, 1)

test_anchor = '''    #[test]\n    fn reads_n_max_from_cli() {\n'''
test = '''    #[test]\n    fn recognizes_llama_serve_subcommand_and_reads_n_max() {\n        let args = vec![\n            "/home/christopherf/.local/bin/llama".to_string(),\n            "serve".to_string(),\n            "-hf".to_string(),\n            "unsloth/Qwen3.8-27B-GGUF:UD-Q4_K_M".to_string(),\n            "--port".to_string(),\n            "8081".to_string(),\n            "--spec-type".to_string(),\n            "draft-mtp".to_string(),\n            "--spec-draft-n-max".to_string(),\n            "3".to_string(),\n        ];\n\n        assert!(is_llama_server_process(&args));\n        assert!(command_targets_port(&args, 8081));\n        let spec = speculative_from_process(&args, None);\n        assert!(spec.enabled);\n        assert!(spec.is_mtp);\n        assert_eq!(spec.n_max, Some(3));\n    }\n\n'''
if test_anchor not in text:
    raise SystemExit('test anchor not found')
text = text.replace(test_anchor, test + test_anchor, 1)

path.write_text(text)
