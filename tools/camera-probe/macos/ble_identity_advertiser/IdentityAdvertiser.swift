import CoreBluetooth
import Foundation

struct Config {
    let name: String
    let duration: TimeInterval
    let advertiseGapService: Bool
}

func usage() {
    print("""
    Usage:
      macos-ble-identity-advertiser --name NAME [--duration SEC] [--no-advertise-gap-service]

    Advertises a BLE Local Name and exposes Generic Access Device Name 0x2A00.
    """)
}

func fail(_ message: String) -> Never {
    FileHandle.standardError.write((message + "\n").data(using: .utf8)!)
    exit(2)
}

func parseConfig() -> Config {
    var name = ""
    var duration: TimeInterval = 120
    var advertiseGapService = true
    var args = Array(CommandLine.arguments.dropFirst())

    while !args.isEmpty {
        let arg = args.removeFirst()
        switch arg {
        case "--name":
            guard let value = args.first else { fail("--name requires a value") }
            name = value
            args.removeFirst()
        case "--duration":
            guard let value = args.first else { fail("--duration requires a value") }
            guard let parsed = TimeInterval(value), parsed >= 0 else {
                fail("--duration must be a non-negative number")
            }
            duration = parsed
            args.removeFirst()
        case "--no-advertise-gap-service":
            advertiseGapService = false
        case "-h", "--help":
            usage()
            exit(0)
        default:
            fail("unknown argument: \(arg)")
        }
    }

    if name.isEmpty {
        fail("--name is required")
    }
    guard name.data(using: .utf8) != nil else {
        fail("--name must be UTF-8 encodable")
    }
    return Config(name: name, duration: duration, advertiseGapService: advertiseGapService)
}

final class IdentityAdvertiser: NSObject, CBPeripheralManagerDelegate {
    private let config: Config
    private let deviceNameUUID = CBUUID(string: "2A00")
    private let gapServiceUUID = CBUUID(string: "1800")
    private var manager: CBPeripheralManager!
    private var deviceNameData: Data
    private var gapServiceAdded = false

    init(config: Config) {
        self.config = config
        self.deviceNameData = Data(config.name.utf8)
        super.init()
    }

    func start() {
        log("starting CoreBluetooth peripheral manager name=\(config.name) duration=\(config.duration)")
        manager = CBPeripheralManager(delegate: self, queue: .main)
        if config.duration > 0 {
            DispatchQueue.main.asyncAfter(deadline: .now() + config.duration) { [weak self] in
                self?.stopAndExit()
            }
        }
    }

    private func stopAndExit() {
        if manager.isAdvertising {
            manager.stopAdvertising()
        }
        log("advertiser stopped")
        exit(0)
    }

    private func addGapService() {
        let deviceName = CBMutableCharacteristic(
            type: deviceNameUUID,
            properties: [.read],
            value: nil,
            permissions: [.readable]
        )
        let service = CBMutableService(type: gapServiceUUID, primary: true)
        service.characteristics = [deviceName]
        manager.add(service)
        log("adding GAP service uuid=1800 characteristic=2A00")
    }

    private func startAdvertising() {
        var advertisement: [String: Any] = [
            CBAdvertisementDataLocalNameKey: config.name
        ]
        if config.advertiseGapService && gapServiceAdded {
            advertisement[CBAdvertisementDataServiceUUIDsKey] = [gapServiceUUID]
        }
        log("starting advertisement local_name=\(config.name) advertise_gap_service=\(config.advertiseGapService && gapServiceAdded)")
        manager.startAdvertising(advertisement)
    }

    private func log(_ message: String) {
        print(message)
        fflush(stdout)
    }

    func peripheralManagerDidUpdateState(_ peripheral: CBPeripheralManager) {
        log("peripheral_state=\(peripheral.state.rawValue)")
        switch peripheral.state {
        case .poweredOn:
            addGapService()
        case .unsupported:
            fail("CoreBluetooth peripheral role is unsupported on this Mac")
        case .unauthorized:
            fail("Bluetooth permission is not authorized for this process")
        case .poweredOff:
            fail("Bluetooth is powered off")
        default:
            break
        }
    }

    func peripheralManager(_ peripheral: CBPeripheralManager, didAdd service: CBService, error: Error?) {
        if let error {
            log("gap_service_add_failed uuid=1800 error=\(error.localizedDescription)")
            log("falling back to Local Name advertisement without readable GAP 0x2A00")
            startAdvertising()
            return
        }
        gapServiceAdded = true
        log("service_added uuid=\(service.uuid.uuidString)")
        startAdvertising()
    }

    func peripheralManagerDidStartAdvertising(_ peripheral: CBPeripheralManager, error: Error?) {
        if let error {
            fail("failed to start advertising: \(error.localizedDescription)")
        }
        log("advertising_started name=\(config.name)")
    }

    func peripheralManager(_ peripheral: CBPeripheralManager, didReceiveRead request: CBATTRequest) {
        guard request.characteristic.uuid == deviceNameUUID else {
            log("read_unknown uuid=\(request.characteristic.uuid.uuidString)")
            peripheral.respond(to: request, withResult: .attributeNotFound)
            return
        }

        let offset = request.offset
        guard offset <= deviceNameData.count else {
            log("read_gap_device_name_invalid_offset offset=\(offset) length=\(deviceNameData.count)")
            peripheral.respond(to: request, withResult: .invalidOffset)
            return
        }

        request.value = deviceNameData.subdata(in: offset..<deviceNameData.count)
        log("read_gap_device_name offset=\(offset) value=\(config.name)")
        peripheral.respond(to: request, withResult: .success)
    }
}

let advertiser = IdentityAdvertiser(config: parseConfig())
advertiser.start()
RunLoop.main.run()
