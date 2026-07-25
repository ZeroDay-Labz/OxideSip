fn main() {
    println!("inputs:");
    for d in softphone_media::devices::list_input_devices().expect("list inputs") {
        println!("  {} — {}", d.id, d.description);
    }
    println!("outputs:");
    for d in softphone_media::devices::list_output_devices().expect("list outputs") {
        println!("  {} — {}", d.id, d.description);
    }
}
