import Foundation

final class ShelfClient {
    let session = URLSession.shared

    func loadShelf(id: String) async throws -> Data {
        let url = URL(string: "https://api.example.com/v1/shelves/\(id)")!
        let (data, _) = try await session.data(from: url)
        return data
    }

    func restock(id: String) async throws {
        var request = URLRequest(url: URL(string: "https://api.example.com/v1/shelves/\(id)/restock")!)
        request.httpMethod = "POST"
        _ = try await session.data(for: request)
    }
}
