/*
 * iOS host bootstrap for the Choreographr GUI.
 *
 * OWNERSHIP OF LAUNCH (the PHASE 0B fix): winit 0.30's iOS event loop
 * *itself* calls UIApplicationMain from EventLoop::run_app, and it asserts
 * at that point that UIApplication::sharedApplication is still nil
 * ("`EventLoop` cannot be `run` after a call to `UIApplicationMain` on
 * iOS"). It deliberately passes None for both the application class and the
 * delegate, so that the embedder can supply a custom delegate via Info.plist
 * if ever needed — no delegate is required for a plain app.
 *
 * That means this bootstrap must NOT call UIApplicationMain (the previous
 * PHASE 0B design did, from a custom UIApplicationDelegate, and the Rust
 * event loop launched inside application:didFinishLaunchingWithOptions:
 * would hit winit's sharedApplication assert and die at launch). Instead
 * `main()` simply hands control to the Rust staticlib immediately: the
 * Dioxus Native / blitz-shell stack creates the winit event loop on the
 * main thread and run_app starts UIApplicationMain itself. All UI-relevant
 * UIKit init (winit issue #1705) has happened by the time the app's windows
 * are created — blitz-shell does that from ApplicationHandler::resumed,
 * which is the point the docs require window creation to happen at.
 *
 * The staticlib is produced by scripts/build-ios.sh from the workspace rlib;
 * link it into this app target via the Xcode project (project.yml).
 */

/* Provided by the Rust staticlib (choreo-gui, cfg(target_os = "ios")). */
extern void choreo_gui_ios_main(void);

int main(int argc, char *argv[]) {
    /* argc/argv are forwarded implicitly: winit's EventLoop::run pulls them
     * from _NSGetArgc/_NSGetArgv when it calls UIApplicationMain. */
    (void)argc;
    (void)argv;
    choreo_gui_ios_main();
    return 0;
}
