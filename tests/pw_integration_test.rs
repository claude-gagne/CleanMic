//! PipeWire integration tests.
//!
//! These tests require a running PipeWire daemon and are marked `#[ignore]`
//! so they only run when explicitly requested:
//!
//! ```sh
//! cargo test --features pipewire --test pw_integration_test -- --ignored
//! ```

#[cfg(feature = "pipewire")]
mod pw_integration {
    use std::time::Duration;

    /// Query `pw-link` for output and input ports matching `name`.
    fn ports_matching(name: &str) -> Vec<String> {
        let out = std::process::Command::new("pw-link")
            .args(["-o"])
            .output()
            .expect("pw-link -o");
        let stdout = String::from_utf8_lossy(&out.stdout).to_string();

        let inp = std::process::Command::new("pw-link")
            .args(["-i"])
            .output()
            .expect("pw-link -i");
        let stdin_out = String::from_utf8_lossy(&inp.stdout).to_string();

        stdout
            .lines()
            .chain(stdin_out.lines())
            .filter(|l| l.contains(name))
            .map(|s| s.to_string())
            .collect()
    }

    /// The virtual mic created by `PipeWireManager` should have visible ports
    /// in the PipeWire graph (verifiable via `pw-link`).
    #[test]
    #[ignore]
    fn virtual_mic_has_visible_ports() {
        use cleanmic::pipewire::PipeWireManager;

        let mut manager = PipeWireManager::connect().unwrap();
        let _capture_reader = manager.take_capture_reader().unwrap();
        let _output_writer = manager.take_output_writer().unwrap();

        manager.create_virtual_mic(None).unwrap();

        // Give PipeWire and WirePlumber time to register ports.
        std::thread::sleep(Duration::from_secs(3));

        let ports = ports_matching("CleanMic");
        eprintln!("CleanMic ports:");
        for p in &ports {
            eprintln!("  {}", p);
        }

        // The virtual mic node should have a capture port (output) and an
        // input port. The capture port is what browsers read from.
        let has_capture = ports.iter().any(|p| p.contains("capture_MONO"));
        let has_input = ports.iter().any(|p| p.contains("input_MONO"));

        // The output stream should have a port for writing processed audio.
        let output_ports = ports_matching("CleanMic-output");
        let has_output_stream = !output_ports.is_empty();

        // The capture stream should have a port for reading from the mic.
        let capture_ports = ports_matching("CleanMic-capture");
        let has_capture_stream = !capture_ports.is_empty();

        // Clean up.
        manager.destroy_virtual_mic().unwrap();
        std::thread::sleep(Duration::from_millis(500));

        // Verify ports are gone after destroy.
        let ports_after = ports_matching("CleanMic");

        assert!(
            has_capture,
            "Virtual mic should have a capture_MONO port (browsers read from this)"
        );
        assert!(
            has_input,
            "Virtual mic should have an input_MONO port (receives processed audio)"
        );
        assert!(
            has_output_stream,
            "Output stream should have a port for writing processed audio"
        );
        assert!(
            has_capture_stream,
            "Capture stream should have a port for reading from the mic"
        );
        assert!(
            ports_after.is_empty(),
            "All CleanMic ports should be removed after destroy"
        );
    }
}
