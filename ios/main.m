/*
 * iOS host bootstrap for the Choreographr GUI.
 *
 * iOS applications do not call a Rust/C symbol directly the way Android's
 * `android_main` does: the UIApplication runtime starts from this `main()`,
 * and winit's iOS backend requires the event loop to be constructed on the
 * main thread once UIKit is running. This bootstrap therefore:
 *
 *   1. registers the Rust staticlib's `choreo_gui_ios_main` trampoline
 *      (choreo-gui/src/lib.rs) to be invoked from the application delegate's
 *      `application:didFinishLaunchingWithOptions:`,
 *   2. calls `UIApplicationMain` to start the UIKit run loop.
 *
 * PHASE 0B CAVEAT: the exact
 * handshake between the delegate and winit's iOS event loop must be verified
 * on a Mac with Xcode (blitz-shell 0.2 has no documented `set_ios_app` slot,
 * unlike its `set_android_app`); this file is deliberately kept minimal so
 * the fix lands in one place if the wiring differs.
 *
 * The staticlib is produced by scripts/build-ios.sh from the workspace rlib;
 * link it into this app target via the Xcode project (project.yml).
 */

#import <UIKit/UIKit.h>

/* Provided by the Rust staticlib (choreo-gui, cfg(target_os = "ios")). */
extern void choreo_gui_ios_main(void);

@interface ChoreographrAppDelegate : UIResponder <UIApplicationDelegate>
@end

@implementation ChoreographrAppDelegate

- (BOOL)application:(UIApplication *)application
    didFinishLaunchingWithOptions:(NSDictionary *)launchOptions {
    (void)application;
    (void)launchOptions;
    /* Hand control to the Dioxus Native app. If winit's iOS backend requires
     * the event loop to be created synchronously inside this callback (the
     * phase 0b open question), this is the line to adjust. */
    choreo_gui_ios_main();
    return YES;
}

@end

int main(int argc, char *argv[]) {
    @autoreleasepool {
        return UIApplicationMain(argc, argv, nil,
                                 NSStringFromClass([ChoreographrAppDelegate class]));
    }
}
