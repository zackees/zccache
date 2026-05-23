use std::fs;

fn libclang_available() -> bool {
    zccache_compiler::arduino::can_load_libclang()
}

#[test]
fn extracts_function_declarations_and_line_numbers_from_ino() {
    if !libclang_available() {
        return;
    }

    let dir = tempfile::tempdir().unwrap();
    let ino = dir.path().join("Blink.ino");
    fs::write(
        &ino,
        r#"
int led = 13;

void setup() {
    helper(42);
}

int helper(int value) {
    return value + 1;
}

static unsigned long tick(unsigned long now = 0) {
    return now + 1;
}
"#,
    )
    .unwrap();

    let generated = zccache_compiler::arduino::generate_ino_cpp(
        &ino,
        &zccache_compiler::arduino::ArduinoConversionOptions::default(),
    )
    .unwrap();

    assert!(
        generated
            .prototypes
            .iter()
            .any(|p| p.declaration == "int helper(int value);"),
        "missing helper prototype: {:?}",
        generated.prototypes
    );
    assert!(
        generated
            .prototypes
            .iter()
            .any(|p| p.declaration == "static unsigned long tick(unsigned long now);"),
        "missing tick prototype with stripped default arg: {:?}",
        generated.prototypes
    );
    assert!(
        generated.prototypes.iter().all(|p| p.line > 0),
        "every extracted prototype should carry a source line number: {:?}",
        generated.prototypes
    );
}

#[test]
fn generated_cpp_contains_arduino_include_and_line_mappings() {
    if !libclang_available() {
        return;
    }

    let dir = tempfile::tempdir().unwrap();
    let ino = dir.path().join("Sketch.ino");
    fs::write(
        &ino,
        r#"
void setup() {}
void loop() {}

int helper(int value) {
    return value + 1;
}
"#,
    )
    .unwrap();

    let generated = zccache_compiler::arduino::generate_ino_cpp(
        &ino,
        &zccache_compiler::arduino::ArduinoConversionOptions::default(),
    )
    .unwrap();

    assert!(generated.cpp.contains("#include <Arduino.h>"));
    assert!(generated.cpp.contains("#line 1 "));
    assert!(generated.cpp.contains("int helper(int value);"));

    let prototype_pos = generated.cpp.find("int helper(int value);").unwrap();
    let body_pos = generated.cpp.find("int helper(int value) {").unwrap();
    assert!(
        prototype_pos < body_pos,
        "prototype must be emitted before original definition"
    );
}
