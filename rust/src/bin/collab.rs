fn main() {
    // Short runs touch little memory; huge pages only add page zeroing.
    #[cfg(target_os = "linux")]
    unsafe {
        libc::prctl(libc::PR_SET_THP_DISABLE, 1, 0, 0, 0);
    }
    collab::set_forget(true);
    let argv: Vec<String> = std::env::args_os().skip(1).map(|a| a.to_string_lossy().into_owned()).collect();
    let code = collab::cli::main(argv);
    // Output is flushed; skip exit handlers, which only tidy memory the
    // kernel is about to reclaim (mimalloc walks every live page here).
    unsafe { libc::_exit(code) }
}
