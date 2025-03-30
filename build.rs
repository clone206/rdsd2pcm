fn main() {
    cc::Build::new()
        .file("src/dsd2pcm.c")
        .compile("dsd2pcm");
}