import AppKit
import AVFoundation
import CoreImage
import Foundation

struct Config {
    let deviceName: String
    let output: String
    let timeout: TimeInterval
    let warmup: TimeInterval
    let zoom: CGFloat?
    let checkPermission: Bool
    let listDevices: Bool
}

func usage() {
    print("""
    Usage:
      camera-capture --device-name NAME --output PATH [--timeout SEC] [--warmup SEC] [--zoom FACTOR]
      camera-capture --check-permission
      camera-capture --list-devices

    Captures one lossless PNG frame from a macOS AVFoundation camera device.
    """)
}

func fail(_ message: String) -> Never {
    FileHandle.standardError.write((message + "\n").data(using: .utf8)!)
    exit(2)
}

func parseConfig() -> Config {
    var deviceName = "iPhone"
    var output = ""
    var timeout: TimeInterval = 10
    var warmup: TimeInterval = 2
    var zoom: CGFloat?
    var checkPermission = false
    var listDevices = false
    var args = Array(CommandLine.arguments.dropFirst())

    while !args.isEmpty {
        let arg = args.removeFirst()
        switch arg {
        case "--device-name":
            guard let value = args.first else { fail("--device-name requires a value") }
            deviceName = value
            args.removeFirst()
        case "--output":
            guard let value = args.first else { fail("--output requires a value") }
            output = value
            args.removeFirst()
        case "--timeout":
            guard let value = args.first, let parsed = TimeInterval(value), parsed > 0 else {
                fail("--timeout requires a positive number")
            }
            timeout = parsed
            args.removeFirst()
        case "--warmup":
            guard let value = args.first, let parsed = TimeInterval(value), parsed >= 0 else {
                fail("--warmup requires a non-negative number")
            }
            warmup = parsed
            args.removeFirst()
        case "--zoom":
            guard let value = args.first, let parsed = Double(value), parsed > 0 else {
                fail("--zoom requires a positive number")
            }
            zoom = CGFloat(parsed)
            args.removeFirst()
        case "--check-permission":
            checkPermission = true
        case "--list-devices":
            listDevices = true
        case "-h", "--help":
            usage()
            exit(0)
        default:
            fail("unknown argument: \(arg)")
        }
    }

    if output.isEmpty && !checkPermission && !listDevices {
        fail("--output is required")
    }
    return Config(
        deviceName: deviceName,
        output: output,
        timeout: timeout,
        warmup: warmup,
        zoom: zoom,
        checkPermission: checkPermission,
        listDevices: listDevices
    )
}

func cameraDeviceTypes() -> [AVCaptureDevice.DeviceType] {
    var types: [AVCaptureDevice.DeviceType] = [.builtInWideAngleCamera, .external]
    if #available(macOS 14.0, *) {
        types.append(.continuityCamera)
    }
    return types
}

func requestCameraAccess(timeout: TimeInterval) -> Bool {
    switch AVCaptureDevice.authorizationStatus(for: .video) {
    case .authorized:
        return true
    case .notDetermined:
        let semaphore = DispatchSemaphore(value: 0)
        var granted = false
        AVCaptureDevice.requestAccess(for: .video) { allowed in
            granted = allowed
            semaphore.signal()
        }
        _ = semaphore.wait(timeout: .now() + timeout)
        return granted
    default:
        return false
    }
}

func cameraAuthorizationLabel() -> String {
    switch AVCaptureDevice.authorizationStatus(for: .video) {
    case .authorized:
        return "authorized"
    case .notDetermined:
        return "not_determined"
    case .denied:
        return "denied"
    case .restricted:
        return "restricted"
    @unknown default:
        return "unknown"
    }
}

func selectDevice(named name: String) -> AVCaptureDevice? {
    let discovery = AVCaptureDevice.DiscoverySession(
        deviceTypes: cameraDeviceTypes(),
        mediaType: .video,
        position: .unspecified
    )
    let folded = name.lowercased()
    if let exact = discovery.devices.first(where: { $0.localizedName.lowercased() == folded }) {
        return exact
    }
    if let partial = discovery.devices.first(where: { $0.localizedName.lowercased().contains(folded) }) {
        return partial
    }
    return discovery.devices.first
}

func printDevices() {
    let discovery = AVCaptureDevice.DiscoverySession(
        deviceTypes: cameraDeviceTypes(),
        mediaType: .video,
        position: .unspecified
    )
    print("camera_authorization=\(cameraAuthorizationLabel())")
    for device in discovery.devices {
        print("device=\(device.localizedName)")
        print("  unique_id=\(device.uniqueID)")
        print("  model_id=\(device.modelID)")
        print("  manufacturer=\(device.manufacturer)")
        print("  connected=\(device.isConnected)")
        print("  suspended=\(device.isSuspended)")
        print("  zoom_control=output_center_crop_only")
    }
}

func configureDevice(_ device: AVCaptureDevice) {
    do {
        try device.lockForConfiguration()
        defer { device.unlockForConfiguration() }
        if device.isFocusModeSupported(.continuousAutoFocus) {
            device.focusMode = .continuousAutoFocus
        }
        if device.isExposureModeSupported(.continuousAutoExposure) {
            device.exposureMode = .continuousAutoExposure
        }
        if device.isWhiteBalanceModeSupported(.continuousAutoWhiteBalance) {
            device.whiteBalanceMode = .continuousAutoWhiteBalance
        }
    } catch {
        FileHandle.standardError.write(
            ("warning: could not configure camera focus/exposure: \(error.localizedDescription)\n").data(using: .utf8)!
        )
    }
}

func applyOutputZoom(_ image: CIImage, zoom: CGFloat?) -> CIImage {
    guard let zoom, zoom > 1 else {
        return image
    }
    let extent = image.extent
    let cropWidth = extent.width / zoom
    let cropHeight = extent.height / zoom
    let cropX = extent.midX - (cropWidth / 2)
    let cropY = extent.midY - (cropHeight / 2)
    let cropRect = CGRect(x: cropX, y: cropY, width: cropWidth, height: cropHeight)
    return image
        .cropped(to: cropRect)
        .transformed(by: CGAffineTransform(translationX: -cropX, y: -cropY))
        .transformed(by: CGAffineTransform(scaleX: zoom, y: zoom))
        .cropped(to: CGRect(origin: .zero, size: extent.size))
}

func writePNG(pixelBuffer: CVPixelBuffer, output: String, zoom: CGFloat?) throws {
    let image = applyOutputZoom(CIImage(cvPixelBuffer: pixelBuffer), zoom: zoom)
    let context = CIContext()
    guard let cgImage = context.createCGImage(image, from: image.extent) else {
        fail("captured frame could not be converted to CGImage")
    }
    let bitmap = NSBitmapImageRep(cgImage: cgImage)
    guard let png = bitmap.representation(using: .png, properties: [:]) else {
        fail("captured frame could not be converted to PNG")
    }
    let url = URL(fileURLWithPath: output)
    try FileManager.default.createDirectory(
        at: url.deletingLastPathComponent(),
        withIntermediateDirectories: true
    )
    try png.write(to: url)
}

func writePNG(photoData: Data, output: String) throws {
    guard let image = NSImage(data: photoData),
          let tiff = image.tiffRepresentation,
          let bitmap = NSBitmapImageRep(data: tiff),
          let png = bitmap.representation(using: .png, properties: [:]) else {
        fail("captured photo could not be converted to PNG")
    }
    let url = URL(fileURLWithPath: output)
    try FileManager.default.createDirectory(
        at: url.deletingLastPathComponent(),
        withIntermediateDirectories: true
    )
    try png.write(to: url)
}

final class CaptureDelegate: NSObject, AVCaptureVideoDataOutputSampleBufferDelegate {
    private let outputPath: String
    private let semaphore: DispatchSemaphore
    private let earliestCaptureAt: Date
    private let zoom: CGFloat?
    private let lock = NSLock()
    private var finished = false
    private(set) var error: String?

    init(outputPath: String, semaphore: DispatchSemaphore, warmup: TimeInterval, zoom: CGFloat?) {
        self.outputPath = outputPath
        self.semaphore = semaphore
        self.earliestCaptureAt = Date().addingTimeInterval(warmup)
        self.zoom = zoom
    }

    private func claimFrame() -> Bool {
        lock.lock()
        defer { lock.unlock() }
        if finished {
            return false
        }
        finished = true
        return true
    }

    func captureOutput(
        _ output: AVCaptureOutput,
        didOutput sampleBuffer: CMSampleBuffer,
        from connection: AVCaptureConnection
    ) {
        if Date() < earliestCaptureAt {
            return
        }
        guard claimFrame() else { return }
        defer { semaphore.signal() }
        guard let pixelBuffer = CMSampleBufferGetImageBuffer(sampleBuffer) else {
            self.error = "captured sample buffer had no image buffer"
            return
        }
        do {
            try writePNG(pixelBuffer: pixelBuffer, output: outputPath, zoom: zoom)
        } catch {
            self.error = error.localizedDescription
        }
    }
}

let config = parseConfig()

if config.checkPermission {
    print("camera_authorization=\(cameraAuthorizationLabel())")
    exit(0)
}

if config.listDevices {
    printDevices()
    exit(0)
}

guard requestCameraAccess(timeout: config.timeout) else {
    fail("camera permission is not authorized for this process")
}

guard let device = selectDevice(named: config.deviceName) else {
    fail("no AVFoundation video devices found")
}
configureDevice(device)

let session = AVCaptureSession()
if session.canSetSessionPreset(.hd1920x1080) {
    session.sessionPreset = .hd1920x1080
} else {
    session.sessionPreset = .high
}

do {
    let input = try AVCaptureDeviceInput(device: device)
    guard session.canAddInput(input) else { fail("cannot add camera input") }
    session.addInput(input)
} catch {
    fail("cannot open camera device \(device.localizedName): \(error.localizedDescription)")
}

let semaphore = DispatchSemaphore(value: 0)
let delegate = CaptureDelegate(
    outputPath: config.output,
    semaphore: semaphore,
    warmup: config.warmup,
    zoom: config.zoom
)
let videoOutput = AVCaptureVideoDataOutput()
videoOutput.videoSettings = [
    kCVPixelBufferPixelFormatTypeKey as String: kCVPixelFormatType_32BGRA
]
videoOutput.alwaysDiscardsLateVideoFrames = true
videoOutput.setSampleBufferDelegate(delegate, queue: DispatchQueue(label: "fuji.camera.capture.frame"))
guard session.canAddOutput(videoOutput) else { fail("cannot add video output") }
session.addOutput(videoOutput)

session.startRunning()
let finished = semaphore.wait(timeout: .now() + config.timeout)
session.stopRunning()

if finished == .timedOut {
    fail("timed out waiting for camera frame")
}
if let error = delegate.error {
    fail(error)
}

print("captured_device=\(device.localizedName)")
if let zoom = config.zoom {
    print("requested_zoom=\(String(format: "%.3f", Double(zoom)))")
    print("applied_zoom=\(String(format: "%.3f", Double(zoom)))")
    print("zoom_mode=output_center_crop")
}
print("output=\(config.output)")
