// Helper for scripts/screenshot.sh and scripts/record.sh: finds Kuana's window, sizes it and brings it to the front.
// Uses only system frameworks so taking screenshots needs no Homebrew packages.
//
//   screenshot-helper window <pid>          print the CGWindowID of the pid's main window
//   screenshot-helper resize <pid> <w> <h>  set that window's size in points (Accessibility)
//   screenshot-helper activate <pid>        bring the app to the front
//   screenshot-helper record <pid> <windowid> <folder> <fps>
//                                           write the window's frames as PNGs until a line on stdin
//   screenshot-helper compose <frames> <out> <width>
//                                           frame them with a margin and a drop shadow

import AppKit
import ApplicationServices
import CoreImage
@preconcurrency import ScreenCaptureKit

func fail(_ message: String) -> Never {
    FileHandle.standardError.write((message + "\n").data(using: .utf8)!)
    exit(1)
}

/// The largest normal-layer, on-screen window of `pid` (Kuana may also have the About window open).
func mainWindow(pid: pid_t) -> CGWindowID? {
    let info = CGWindowListCopyWindowInfo([.optionOnScreenOnly, .excludeDesktopElements], kCGNullWindowID)
        as? [[String: Any]] ?? []
    return info.compactMap { window -> (CGWindowID, CGFloat)? in
        guard (window[kCGWindowOwnerPID as String] as? pid_t) == pid,
              (window[kCGWindowLayer as String] as? Int) == 0,
              let id = window[kCGWindowNumber as String] as? CGWindowID,
              let bounds = window[kCGWindowBounds as String] as? NSDictionary,
              let rect = CGRect(dictionaryRepresentation: bounds),
              rect.width > 100, rect.height > 100
        else { return nil }
        return (id, rect.width * rect.height)
    }.max { $0.1 < $1.1 }?.0
}

func resize(pid: pid_t, width: CGFloat, height: CGFloat) {
    guard AXIsProcessTrusted() else {
        fail("can't resize: give your terminal Accessibility access (System Settings → Privacy & Security)")
    }
    let app = AXUIElementCreateApplication(pid)
    var value: CFTypeRef?
    guard AXUIElementCopyAttributeValue(app, kAXMainWindowAttribute as CFString, &value) == .success,
          let window = value
    else { fail("can't find the window to resize") }
    var size = CGSize(width: width, height: height)
    let axSize = AXValueCreate(.cgSize, &size)!
    AXUIElementSetAttributeValue(window as! AXUIElement, kAXSizeAttribute as CFString, axSize)
}

/// Set once, from any thread.
final class Flag: @unchecked Sendable {
    private let lock = NSLock()
    private var value = false
    func set() { lock.lock(); value = true; lock.unlock() }
    var isSet: Bool { lock.lock(); defer { lock.unlock() }; return value }
}

/// Writes the window's frames as `frame-00001.png`… into `folder`, about `fps` a second, until a
/// line (or the end) arrives on stdin, and each frame's time in milliseconds to `times.txt`.
/// Captures only that window, with its rounded corners on a transparent background, at the
/// display's resolution, without the pointer, whatever is in front of it (`compose` adds the
/// margin and shadow). Takes a screenshot per frame instead of streaming: while a stream records a
/// window, macOS draws its sharing badge over the window's buttons.
func record(window id: CGWindowID, to folder: URL, fps: Int32) {
    _ = NSApplication.shared  // connects to the window server, which screenshots need
    let stop = Flag()
    DispatchQueue.global().async {
        _ = readLine()
        stop.set()
    }
    let writer = DispatchQueue(label: "frames", qos: .userInitiated, attributes: .concurrent)
    Task {
        do {
            let content = try await SCShareableContent.excludingDesktopWindows(false, onScreenWindowsOnly: true)
            guard let window = content.windows.first(where: { $0.windowID == id }) else { fail("no window \(id) to record") }
            let filter = SCContentFilter(desktopIndependentWindow: window)
            let config = SCStreamConfiguration()
            let scale = CGFloat(filter.pointPixelScale)
            config.width = Int(filter.contentRect.width * scale)
            config.height = Int(filter.contentRect.height * scale)
            config.showsCursor = false
            config.ignoreShadowsSingleWindow = true
            config.shouldBeOpaque = false
            let interval = 1.0 / Double(fps)
            let start = Date()
            var times: [Int] = []
            print("recording")
            fflush(stdout)
            while !stop.isSet {
                let began = Date()
                let image = try await SCScreenshotManager.captureImage(contentFilter: filter, configuration: config)
                times.append(Int(began.timeIntervalSince(start) * 1000))
                let url = folder.appendingPathComponent(String(format: "frame-%05d.png", times.count))
                writer.async {
                    guard let out = CGImageDestinationCreateWithURL(url as CFURL, "public.png" as CFString, 1, nil) else { return }
                    CGImageDestinationAddImage(out, image, nil)
                    CGImageDestinationFinalize(out)
                }
                let rest = interval - Date().timeIntervalSince(began)
                if rest > 0 { try await Task.sleep(nanoseconds: UInt64(rest * 1e9)) }
            }
            writer.sync(flags: .barrier) {}
            try times.map(String.init).joined(separator: "\n").write(to: folder.appendingPathComponent("times.txt"), atomically: true, encoding: .utf8)
            exit(0)
        } catch {
            fail("can't record: \(error.localizedDescription). The app running this script needs Screen Recording access.")
        }
    }
    dispatchMain()
}


/// Puts each `frame-*.png` of `input` on a transparent canvas with a margin and a soft drop
/// shadow, as `screencapture -l` frames a window, scaled so the window is `width` pixels wide;
/// writes them under the same names into `output`.
func compose(_ input: URL, into output: URL, width: CGFloat) {
    let names = ((try? FileManager.default.contentsOfDirectory(atPath: input.path)) ?? [])
        .filter { $0.hasPrefix("frame-") && $0.hasSuffix(".png") }
        .sorted()
    DispatchQueue.concurrentPerform(iterations: names.count) { ix in
        let name = names[ix]
        guard let source = CGImageSourceCreateWithURL(input.appendingPathComponent(name) as CFURL, nil),
              let frame = CGImageSourceCreateImageAtIndex(source, 0, nil)
        else { return }
        let scale = width / CGFloat(frame.width)
        let window = CGSize(width: width, height: (CGFloat(frame.height) * scale).rounded())
        // Room for the shadow, which falls a little downward.
        let margin = (width * 0.045).rounded()
        let size = CGSize(width: window.width + 2 * margin, height: window.height + 2 * margin)
        guard let context = CGContext(
            data: nil, width: Int(size.width), height: Int(size.height), bitsPerComponent: 8, bytesPerRow: 0,
            space: CGColorSpace(name: CGColorSpace.sRGB)!, bitmapInfo: CGImageAlphaInfo.premultipliedLast.rawValue)
        else { return }
        context.interpolationQuality = .high
        context.setShadow(offset: CGSize(width: 0, height: -margin * 0.3), blur: margin * 0.8,
                          color: CGColor(gray: 0, alpha: 0.45))
        context.draw(frame, in: CGRect(origin: CGPoint(x: margin, y: margin), size: window))
        guard let image = context.makeImage(),
              let out = CGImageDestinationCreateWithURL(output.appendingPathComponent(name) as CFURL, "public.png" as CFString, 1, nil)
        else { return }
        CGImageDestinationAddImage(out, image, nil)
        CGImageDestinationFinalize(out)
    }
}

let args = CommandLine.arguments
func pidArg() -> pid_t {
    guard args.count > 2, let pid = pid_t(args[2]) else { fail("expected a pid") }
    return pid
}

switch args.count > 1 ? args[1] : "" {
case "window":
    guard let window = mainWindow(pid: pidArg()) else { exit(1) }
    print(window)
case "resize":
    guard args.count == 5, let w = Double(args[3]), let h = Double(args[4]) else { fail("usage: resize <pid> <w> <h>") }
    resize(pid: pidArg(), width: w, height: h)
case "record":
    guard args.count == 6, let id = CGWindowID(args[3]), let fps = Int32(args[5]) else {
        fail("usage: record <pid> <windowid> <frames folder> <fps>")
    }
    record(window: id, to: URL(fileURLWithPath: args[4]), fps: fps)
case "compose":
    guard args.count == 5, let width = Double(args[4]) else { fail("usage: compose <frames folder> <output folder> <width>") }
    compose(URL(fileURLWithPath: args[2]), into: URL(fileURLWithPath: args[3]), width: CGFloat(width))
case "activate":
    let pid = pidArg()
    // Since macOS 14, NSRunningApplication.activate is only a request that the frontmost app
    // (the terminal) may ignore; Accessibility's frontmost attribute is honored.
    if AXIsProcessTrusted() {
        AXUIElementSetAttributeValue(AXUIElementCreateApplication(pid), kAXFrontmostAttribute as CFString, kCFBooleanTrue)
    } else {
        guard let app = NSRunningApplication(processIdentifier: pid) else { fail("no such process") }
        app.activate()
    }
default:
    fail("usage: screenshot-helper window|resize|activate …")
}
