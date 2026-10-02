import Foundation

struct BeaconService {
    func ping() async throws {
        let url = URL(string: "https://telemetry.example.com/v1/beacons/ping")!
        _ = try await URLSession.shared.data(from: url)
    }
}
