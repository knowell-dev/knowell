import Foundation

struct Paths {
    static let docs = "/v1/docs"

    func fileURL() -> URL {
        URL(fileURLWithPath: "/tmp/cache.json")
    }
}
