// hr-input: the app-owned native input helper for macOS GUI workers (Tart VMs).
//
// One stable, root-owned, signed executable gets the operator's one-time Accessibility grant, so
// native acceptance suites do not have to compile their own helper (a freshly compiled executable
// does not inherit an older executable's TCC grant). Build, sign and install it with
// images/macos-latest/gui/install-gui.sh; README: "Native GUI workers".
//
//   version                              rev=<sha256 of this source, stamped at build time>
//   access [--prompt]                    {"session":bool,"trusted":bool,"post":bool,"vm":bool}
//   selftest                             proves a real keyboard and pointer event reaches a
//                                        disposable test app; prints one JSON result line
//   activate <pid>                       brings that app to the front, or fails
//   key <pid> <keycode> [cmd|ctrl|alt|shift]...
//   move <x> <y>
//   drag <x1> <y1> <x2> <y2> [dwell-ms]
//   exclusive -- <command> [args...]     runs the command holding this worker's GUI lock, so two
//                                        suites in one job cannot fight over focus
//
// `key`, `move`, `drag`, `activate` and `selftest` post to a real desktop, so they refuse to run
// anywhere but inside a virtual machine (kern.hv_vmm_present) unless HR_INPUT_ALLOW_BARE_METAL=1
// is set: they must never move an operator's own keyboard focus or pointer. When they run outside
// the login session (over SSH, for instance) they re-run themselves inside it with
// `sudo -n launchctl asuser`.
//
// Coordinates are global logical points, top-left of the primary display, as
// window.screenX/screenY report them. Exit codes: 0 ok, 2 failed.
import AppKit
import ApplicationServices
import Foundation

func fail(_ message: String) -> Never {
  FileHandle.standardError.write((message + "\n").data(using: .utf8)!)
  exit(2)
}
func number(_ value: String) -> Double {
  guard let parsed = Double(value), parsed.isFinite else { fail("not a number: \(value)") }
  return parsed
}
func pause(_ milliseconds: Double) { usleep(useconds_t(milliseconds * 1000)) }

func json(_ fields: [(String, Any)]) -> String {
  let body = fields.map { key, value -> String in
    switch value {
    case let flag as Bool: return "\"\(key)\":\(flag)"
    default:
      let text = String(describing: value)
        .replacingOccurrences(of: "\\", with: "\\\\")
        .replacingOccurrences(of: "\"", with: "\\\"")
        .replacingOccurrences(of: "\n", with: " ")
      return "\"\(key)\":\"\(text)\""
    }
  }
  return "{" + body.joined(separator: ",") + "}"
}

func insideVirtualMachine() -> Bool {
  var value: Int32 = 0
  var size = MemoryLayout<Int32>.size
  return sysctlbyname("kern.hv_vmm_present", &value, &size, nil, 0) == 0 && value == 1
}

/// The login session this process can reach: signed in on the console, past the login window.
func sessionState() -> (reachable: Bool, detail: String) {
  guard let dict = CGSessionCopyCurrentDictionary() as? [String: Any] else {
    return (false, "this process is outside any login session (no WindowServer connection)")
  }
  let onConsole = (dict["kCGSSessionOnConsoleKey"] as? Bool) ?? false
  let loginDone = (dict["kCGSessionLoginDoneKey"] as? Bool) ?? false
  if !loginDone { return (false, "the console is still at the login window") }
  if !onConsole { return (false, "the login session is not the console session") }
  return (true, "console session of \(dict["kCGSSessionUserNameKey"] ?? "?")")
}

// MARK: running inside the login session

/// Re-runs this command inside the user's login session when it was started outside it. Returns
/// only if it is already inside (or cannot hop, which the caller reports as a session failure).
func enterSessionIfNeeded() -> Bool {
  if sessionState().reachable || ProcessInfo.processInfo.environment["HR_INPUT_HOPPED"] != nil {
    return true
  }
  func run(_ args: [String]) -> Int32 {
    let task = Process()
    task.executableURL = URL(fileURLWithPath: args[0])
    task.arguments = Array(args.dropFirst())
    task.standardOutput = FileHandle.nullDevice
    task.standardError = FileHandle.nullDevice
    do { try task.run() } catch { return 127 }
    task.waitUntilExit()
    return task.terminationStatus
  }
  guard run(["/usr/bin/sudo", "-n", "/usr/bin/true"]) == 0 else { return false }
  let uid = getuid()
  let user = NSUserName()
  let me = CommandLine.arguments[0].hasPrefix("/")
    ? CommandLine.arguments[0] : (Bundle.main.executablePath ?? CommandLine.arguments[0])
  let argv =
    ["sudo", "-n", "/bin/launchctl", "asuser", String(uid), "/usr/bin/sudo", "-n", "-u", user,
      "/usr/bin/env", "HR_INPUT_HOPPED=1", me] + CommandLine.arguments.dropFirst()
  var cargs: [UnsafeMutablePointer<CChar>?] = argv.map { strdup($0) } + [nil]
  execv("/usr/bin/sudo", &cargs)
  return false
}

func requireWorkerSession() {
  if !insideVirtualMachine() && ProcessInfo.processInfo.environment["HR_INPUT_ALLOW_BARE_METAL"] != "1" {
    fail("refusing to post input: this is not a virtual machine, so the events would go to a real desktop. Run it in a Tart worker (HR_INPUT_ALLOW_BARE_METAL=1 overrides, for development).")
  }
  guard enterSessionIfNeeded() else {
    fail("no login session is reachable from this process, and passwordless sudo is unavailable to enter it")
  }
  let state = sessionState()
  if !state.reachable { fail("no login session: \(state.detail)") }
}

// MARK: input

let source = CGEventSource(stateID: .hidSystemState)
func post(_ event: CGEvent?) {
  guard let event else { fail("could not create an input event") }
  event.post(tap: .cghidEventTap)
}
func mouse(_ type: CGEventType, _ point: CGPoint) {
  let event = CGEvent(
    mouseEventSource: source, mouseType: type, mouseCursorPosition: point, mouseButton: .left)
  if type == .leftMouseDown || type == .leftMouseUp || type == .leftMouseDragged {
    event?.setIntegerValueField(.mouseEventClickState, value: 1)
  }
  post(event)
}

enum Failure: Error { case focus(String), permission(String) }

/// Brings `pid` to the front and checks it got there. Bounded by a count of run-loop turns, not
/// by elapsed time.
func activate(_ pid: pid_t) throws {
  guard pid > 0, let app = NSRunningApplication(processIdentifier: pid), !app.isTerminated
  else { throw Failure.focus("activate <pid>: no such app") }
  app.activate(options: [.activateAllWindows])
  _ = AXUIElementSetAttributeValue(
    AXUIElementCreateApplication(pid), kAXFrontmostAttribute as CFString, kCFBooleanTrue)
  for _ in 0..<50 where NSWorkspace.shared.frontmostApplication?.processIdentifier != pid {
    RunLoop.current.run(until: Date().addingTimeInterval(0.1))
  }
  let front = NSWorkspace.shared.frontmostApplication?.processIdentifier ?? 0
  if front != pid {
    throw Failure.focus("target \(pid) did not come to the front (frontmost PID \(front))")
  }
}

func modifiers(_ names: [String]) -> CGEventFlags {
  var flags = CGEventFlags()
  for modifier in names {
    switch modifier {
    case "cmd": flags.insert(.maskCommand)
    case "ctrl": flags.insert(.maskControl)
    case "alt": flags.insert(.maskAlternate)
    case "shift": flags.insert(.maskShift)
    default: fail("unknown modifier \(modifier)")
    }
  }
  return flags
}

func pressKey(_ pid: pid_t, _ code: UInt16, _ flags: CGEventFlags) {
  for down in [true, false] {
    guard let event = CGEvent(keyboardEventSource: source, virtualKey: code, keyDown: down)
    else { fail("could not create a keyboard event") }
    event.flags = flags
    event.postToPid(pid)
    pause(30)
  }
}

func dragMouse(from: CGPoint, to: CGPoint, dwell: Double) {
  mouse(.mouseMoved, from)
  pause(150)
  mouse(.leftMouseDown, from)
  pause(150)
  // Leave the drag threshold slowly so the page starts a drag, then travel to the target.
  for step in 1...6 {
    mouse(.leftMouseDragged, CGPoint(x: from.x + Double(step) * 2, y: from.y + Double(step) * 2))
    pause(40)
  }
  let steps = 40
  for step in 1...steps {
    let t = Double(step) / Double(steps)
    mouse(
      .leftMouseDragged,
      CGPoint(x: from.x + (to.x - from.x) * t, y: from.y + (to.y - from.y) * t))
    pause(25)
  }
  // Hover over the target so it sees dragenter and dragover before the drop.
  for wiggle in 0..<Int(max(1, dwell / 50)) {
    mouse(.leftMouseDragged, CGPoint(x: to.x + Double(wiggle % 2), y: to.y))
    pause(50)
  }
  mouse(.leftMouseUp, to)
  pause(150)
}

func access(prompt: Bool) -> (trusted: Bool, post: Bool) {
  if prompt {
    _ = CGRequestPostEventAccess()
    _ = AXIsProcessTrustedWithOptions(
      [kAXTrustedCheckOptionPrompt.takeUnretainedValue() as String: true] as CFDictionary)
  }
  return (AXIsProcessTrusted(), CGPreflightPostEventAccess())
}

// MARK: the disposable test app

final class ProbeView: NSView {
  override var acceptsFirstResponder: Bool { true }
  override func acceptsFirstMouse(for event: NSEvent?) -> Bool { true }
  func say(_ text: String) {
    FileHandle.standardOutput.write((text + "\n").data(using: .utf8)!)
  }
  override func keyDown(with event: NSEvent) { say("key \(event.keyCode)") }
  override func mouseDown(with event: NSEvent) { say("down") }
  override func mouseUp(with event: NSEvent) { say("up") }
  override func mouseDragged(with event: NSEvent) { say("drag") }
}

final class TargetDelegate: NSObject, NSApplicationDelegate {
  var window: NSWindow!
  var announced = false
  func applicationDidFinishLaunching(_ note: Notification) {
    let view = ProbeView(frame: NSRect(x: 0, y: 0, width: 400, height: 300))
    window = NSWindow(
      contentRect: NSRect(x: 100, y: 200, width: 400, height: 300), styleMask: [.titled],
      backing: .buffered, defer: false)
    window.title = "home-runner input probe"
    window.contentView = view
    window.makeKeyAndOrderFront(nil)
    window.makeFirstResponder(view)
    NSApp.activate(ignoringOtherApps: true)
    NotificationCenter.default.addObserver(
      forName: NSWindow.didBecomeKeyNotification, object: window, queue: .main
    ) { [self] _ in announce() }
    if window.isKeyWindow { announce() }
    // The helper closes our stdin when it is done with us.
    DispatchQueue.global().async {
      while FileHandle.standardInput.availableData.count > 0 {}
      DispatchQueue.main.async { NSApp.terminate(nil) }
    }
  }
  func announce() {
    guard !announced else { return }
    announced = true
    let screen = NSScreen.screens[0].frame
    let frame = window.frame
    let top = screen.height - (frame.origin.y + frame.height)
    FileHandle.standardOutput.write(
      "ready pid=\(getpid()) frame=\(Int(frame.origin.x)),\(Int(top)),\(Int(frame.width)),\(Int(frame.height))\n"
        .data(using: .utf8)!)
  }
}

func runTarget() -> Never {
  let app = NSApplication.shared
  app.setActivationPolicy(.regular)
  let delegate = TargetDelegate()
  app.delegate = delegate
  app.run()
  exit(0)
}

// MARK: selftest

/// Reads one line from `handle`, giving up after `watchdog` seconds (a hang guard, not a pacing
/// delay: the line normally arrives as soon as the test app has seen the event).
func readLine(from handle: FileHandle, watchdog: Double, buffer: inout Data) -> String? {
  let fd = handle.fileDescriptor
  let deadline = Date().addingTimeInterval(watchdog)
  while true {
    if let newline = buffer.firstIndex(of: 10) {
      let line = String(decoding: buffer[buffer.startIndex..<newline], as: UTF8.self)
      buffer.removeSubrange(buffer.startIndex...newline)
      return line
    }
    let remaining = deadline.timeIntervalSinceNow
    if remaining <= 0 { return nil }
    var pfd = pollfd(fd: fd, events: Int16(POLLIN), revents: 0)
    let ready = poll(&pfd, 1, Int32(min(remaining, 1) * 1000))
    if ready > 0 {
      var chunk = [UInt8](repeating: 0, count: 4096)
      let count = read(fd, &chunk, chunk.count)
      if count <= 0 { return nil }
      buffer.append(contentsOf: chunk[0..<count])
    }
  }
}

func finish(ok: Bool, stage: String, detail: String) -> Never {
  print(json([("ok", ok), ("stage", stage), ("detail", detail)]))
  exit(ok ? 0 : 2)
}

func selftest() -> Never {
  if !insideVirtualMachine() && ProcessInfo.processInfo.environment["HR_INPUT_ALLOW_BARE_METAL"] != "1" {
    finish(ok: false, stage: "vm", detail: "this is not a virtual machine; refusing to post input to a real desktop")
  }
  guard enterSessionIfNeeded() else {
    finish(ok: false, stage: "session", detail: "no login session is reachable from this process and passwordless sudo is unavailable to enter it")
  }
  let state = sessionState()
  if !state.reachable { finish(ok: false, stage: "session", detail: state.detail) }
  let granted = access(prompt: false)
  if !granted.trusted || !granted.post {
    finish(ok: false, stage: "permission", detail: "Accessibility trusted=\(granted.trusted), event posting=\(granted.post) for \(CommandLine.arguments[0])")
  }

  let target = Process()
  target.executableURL = URL(fileURLWithPath: CommandLine.arguments[0].hasPrefix("/") ? CommandLine.arguments[0] : (Bundle.main.executablePath ?? CommandLine.arguments[0]))
  target.arguments = ["target"]
  let out = Pipe()
  let input = Pipe()
  target.standardOutput = out
  target.standardInput = input
  target.standardError = FileHandle.nullDevice
  do { try target.run() } catch { finish(ok: false, stage: "focus", detail: "could not start the test app: \(error)") }
  func stop() {
    try? input.fileHandleForWriting.close()
    target.waitUntilExit()
  }
  var buffer = Data()
  let handle = out.fileHandleForReading
  var frame = CGRect.zero
  var pid: pid_t = 0
  while true {
    guard let line = readLine(from: handle, watchdog: 60, buffer: &buffer) else {
      stop()
      finish(ok: false, stage: "focus", detail: "the test app never became the active, key window")
    }
    if line.hasPrefix("ready ") {
      let parts = line.split(separator: " ")
      pid = pid_t(parts[1].dropFirst(4)) ?? 0
      let numbers = parts[2].dropFirst(6).split(separator: ",").compactMap { Double($0) }
      if numbers.count == 4 { frame = CGRect(x: numbers[0], y: numbers[1], width: numbers[2], height: numbers[3]) }
      break
    }
  }
  do { try activate(pid) } catch {
    stop()
    finish(ok: false, stage: "focus", detail: "\(error)")
  }

  func expect(_ what: String, _ matches: (String) -> Bool) {
    while true {
      guard let line = readLine(from: handle, watchdog: 30, buffer: &buffer) else {
        stop()
        finish(ok: false, stage: "delivery", detail: "the \(what) event was posted but never reached the test app (pid \(pid))")
      }
      if matches(line) { return }
    }
  }
  let centre = CGPoint(x: frame.midX, y: frame.midY)
  pressKey(pid, 0, CGEventFlags())
  expect("keyboard") { $0 == "key 0" }
  mouse(.mouseMoved, centre)
  mouse(.leftMouseDown, centre)
  mouse(.leftMouseUp, centre)
  expect("pointer click") { $0 == "up" }
  dragMouse(
    from: CGPoint(x: centre.x - 60, y: centre.y), to: CGPoint(x: centre.x + 60, y: centre.y),
    dwell: 100)
  expect("pointer drag") { $0 == "drag" }
  stop()
  finish(ok: true, stage: "done", detail: "keyboard, click and drag reached the test app (pid \(pid))")
}

// MARK: commands

var arguments = Array(CommandLine.arguments.dropFirst())
guard !arguments.isEmpty else {
  fail("usage: version | access | selftest | activate | key | move | drag | exclusive")
}
let command = arguments.removeFirst()
switch command {
case "version":
  print("rev=\(helperRevision)")
case "access":
  if !insideVirtualMachine() && ProcessInfo.processInfo.environment["HR_INPUT_ALLOW_BARE_METAL"] != "1" {
    print(json([("session", false), ("trusted", false), ("post", false), ("vm", false)]))
    exit(0)
  }
  guard enterSessionIfNeeded() else {
    print(json([("session", false), ("trusted", false), ("post", false), ("vm", true),
      ("detail", "no login session is reachable and passwordless sudo is unavailable to enter it")]))
    exit(0)
  }
  let state = sessionState()
  let granted = access(prompt: arguments.contains("--prompt"))
  print(json([("session", state.reachable), ("trusted", granted.trusted), ("post", granted.post),
    ("vm", true), ("detail", state.detail)]))
case "selftest":
  selftest()
case "target":
  runTarget()
case "activate":
  requireWorkerSession()
  guard arguments.count == 1, let pid = pid_t(arguments[0]), pid > 0
  else { fail("activate <pid>") }
  do { try activate(pid) } catch { fail("\(error)") }
case "key":
  requireWorkerSession()
  guard arguments.count >= 2, let pid = pid_t(arguments.removeFirst()), pid > 0
  else { fail("key <pid> <keycode> [modifiers]") }
  guard let code = arguments.first.flatMap(UInt16.init) else { fail("key <keycode> [modifiers]") }
  let flags = modifiers(Array(arguments.dropFirst()))
  do { try activate(pid) } catch { fail("\(error)") }
  pressKey(pid, code, flags)
case "move":
  requireWorkerSession()
  guard arguments.count == 2 else { fail("move <x> <y>") }
  mouse(.mouseMoved, CGPoint(x: number(arguments[0]), y: number(arguments[1])))
case "drag":
  requireWorkerSession()
  guard arguments.count >= 4 else { fail("drag <x1> <y1> <x2> <y2> [dwell-ms]") }
  dragMouse(
    from: CGPoint(x: number(arguments[0]), y: number(arguments[1])),
    to: CGPoint(x: number(arguments[2]), y: number(arguments[3])),
    dwell: arguments.count > 4 ? number(arguments[4]) : 400)
case "exclusive":
  guard arguments.first == "--", arguments.count >= 2 else { fail("exclusive -- <command> [args...]") }
  let fd = open("/tmp/home-runner-gui.lock", O_CREAT | O_RDWR, 0o666)
  guard fd >= 0, flock(fd, LOCK_EX) == 0 else { fail("could not take the GUI lock") }
  // The lock belongs to the open file description, which the command inherits across exec, so it
  // is held until the command exits.
  var cargs: [UnsafeMutablePointer<CChar>?] = arguments.dropFirst().map { strdup($0) } + [nil]
  execvp(cargs[0]!, &cargs)
  fail("could not run \(arguments[1])")
default:
  fail("unknown command \(command)")
}
