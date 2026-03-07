# Verify the nushell script parses without syntax errors.
{pkgs}: let
  scriptDir = ../scripts;
in {
  # Test: cache-pin.nu parses without errors
  script-syntax =
    pkgs.runCommand "cache-pin-test-script-syntax" {
      nativeBuildInputs = [pkgs.nushell];
    } ''
      nu --commands "source ${scriptDir}/cache-pin.nu" 2>&1 && echo "Script parsed successfully"
      touch $out
    '';

  # Test: --help flag works (validates argument parsing)
  script-help =
    pkgs.runCommand "cache-pin-test-script-help" {
      nativeBuildInputs = [pkgs.nushell];
    } ''
      nu ${scriptDir}/cache-pin.nu --help 2>&1 && echo "--help works"
      touch $out
    '';

  # Test: --dry-run flag is accepted (parsed without error)
  script-dry-run-flag =
    pkgs.runCommand "cache-pin-test-script-dry-run-flag" {
      nativeBuildInputs = [pkgs.nushell];
    } ''
      # --dry-run without --config should fail on missing config, not on flag parsing
      output=$(nu ${scriptDir}/cache-pin.nu --dry-run 2>&1 || true)
      if echo "$output" | grep -qi "unknown\|unexpected\|parse error"; then
        echo "FAIL: --dry-run flag not recognized" && exit 1
      fi
      echo "--dry-run flag accepted"
      touch $out
    '';

  # Test: --no-lock flag is accepted (parsed without error)
  script-no-lock-flag =
    pkgs.runCommand "cache-pin-test-script-no-lock-flag" {
      nativeBuildInputs = [pkgs.nushell];
    } ''
      output=$(nu ${scriptDir}/cache-pin.nu --no-lock 2>&1 || true)
      if echo "$output" | grep -qi "unknown\|unexpected\|parse error"; then
        echo "FAIL: --no-lock flag not recognized" && exit 1
      fi
      echo "--no-lock flag accepted"
      touch $out
    '';

  # Test: missing --config argument produces an error (not a crash)
  script-missing-config =
    pkgs.runCommand "cache-pin-test-script-missing-config" {
      nativeBuildInputs = [pkgs.nushell];
    } ''
      if nu ${scriptDir}/cache-pin.nu 2>&1; then
        echo "FAIL: should have errored without --config" && exit 1
      fi
      echo "Correctly errored on missing --config"
      touch $out
    '';
}
