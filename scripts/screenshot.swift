// Helper for scripts/screenshot.sh and scripts/record.sh: finds Kuana's window, sizes it and brings it to the front.
// Uses only system frameworks so taking screenshots needs no Homebrew packages.
//
//   screenshot-helper window <pid>          print the CGWindowID of the pid's main window
//   screenshot-helper resize <pid> <w> <h>  set that window's size in points (Accessibility)
//   screenshot-helper activate <pid>        bring the app to the front
//   screenshot-helper record <pid> <windowid> <folder> <fps>
//                                           write the window's frames as PNGs until a line on stdin

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

/// Receives the window's frames from ScreenCaptureKit, keeping the newest. It only sends one
/// when something changed, so a timer writes the newest at a steady rate instead.
final class Frames: NSObject, SCStreamOutput {
    private let lock = NSLock()
    private var newest: CGImage?
    private let context = CIContext()

    func stream(_ stream: SCStream, didOutputSampleBuffer buffer: CMSampleBuffer, of type: SCStreamOutputType) {
        guard type == .screen, let pixels = buffer.imageBuffer,
              let attachments = CMSampleBufferGetSampleAttachmentsArray(buffer, createIfNecessary: false) as? [[SCStreamFrameInfo: Any]],
              let raw = attachments.first?[.status] as? Int, SCFrameStatus(rawValue: raw) == .complete,
              let image = context.createCGImage(CIImage(cvPixelBuffer: pixels), from: CIImage(cvPixelBuffer: pixels).extent)
        else { return }
        lock.lock(); newest = image; lock.unlock()
    }

    func take() -> CGImage? {
        lock.lock(); defer { lock.unlock() }
        return newest
    }
}

/// Writes the window's frames as `frame-00001.png`… into `folder`, `fps` a second, until a
/// line (or the end) arrives on stdin. Captures only that window, at the display's resolution,
/// without the pointer, whatever is in front of it.
func record(window id: CGWindowID, to folder: URL, fps: Int32) {
    let frames = Frames()
    let writer = DispatchQueue(label: "frames", qos: .userInitiated, attributes: .concurrent)
    Task {
        do {
            let content = try await SCShareableContent.excludingDesktopWindows(false, onScreenWindowsOnly: true)
            guard let window = content.windows.first(where: { $0.windowID == id }) else { fail("no window \(id) to record") }
            let filter = SCContentFilter(desktopIndependentWindow: window)
            let config = SCStreamConfiguration()
            let scale = CGFloat(filter.pointPixelScale)
            config.width = Int(window.frame.width * scale)
            config.height = Int(window.frame.height * scale)
            config.minimumFrameInterval = CMTime(value: 1, timescale: fps * 2)
            config.showsCursor = false
            config.ignoreShadowsSingleWindow = true
            let stream = SCStream(filter: filter, configuration: config, delegate: nil)
            try stream.addStreamOutput(frames, type: .screen, sampleHandlerQueue: DispatchQueue(label: "stream"))
            try await stream.startCapture()
            var count = 0
            let timer = DispatchSource.makeTimerSource(queue: DispatchQueue(label: "timer"))
            timer.schedule(deadline: .now(), repeating: 1.0 / Double(fps))
            timer.setEventHandler {
                guard let image = frames.take() else { return }
                count += 1
                let url = folder.appendingPathComponent(String(format: "frame-%05d.png", count))
                writer.async {
                    guard let out = CGImageDestinationCreateWithURL(url as CFURL, "public.png" as CFString, 1, nil) else { return }
                    CGImageDestinationAddImage(out, image, nil)
                    CGImageDestinationFinalize(out)
                }
            }
            timer.resume()
            print("recording")
            fflush(stdout)
            DispatchQueue.global().async {
                _ = readLine()
                timer.cancel()
                Task {
                    try? await stream.stopCapture()
                    writer.sync(flags: .barrier) {}
                    exit(0)
                }
            }
        } catch {
            fail("can't record: \(error.localizedDescription). The app running this script needs Screen Recording access.")
        }
    }
    dispatchMain()
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
