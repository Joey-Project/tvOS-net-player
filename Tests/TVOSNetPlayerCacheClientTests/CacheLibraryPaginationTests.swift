import XCTest
@testable import TVOSNetPlayerCacheClient

final class CacheLibraryPaginationTests: XCTestCase {
    func testCollectsItemsAcrossAllPages() async throws {
        var requestedPageTokens: [String] = []
        let pages = [
            "": CacheLibraryItemsPage(items: [.fixture(id: "item-1")], nextPageToken: "page-2"),
            "page-2": CacheLibraryItemsPage(items: [.fixture(id: "item-2")], nextPageToken: "page-3"),
            "page-3": CacheLibraryItemsPage(items: [.fixture(id: "item-3")], nextPageToken: ""),
        ]

        let items = try await collectCacheLibraryItems { pageToken in
            requestedPageTokens.append(pageToken)
            return try XCTUnwrap(pages[pageToken])
        }

        XCTAssertEqual(requestedPageTokens, ["", "page-2", "page-3"])
        XCTAssertEqual(items.map(\.id), ["item-1", "item-2", "item-3"])
    }

    func testThrowsWhenServerRepeatsPageToken() async {
        do {
            _ = try await collectCacheLibraryItems { _ in
                CacheLibraryItemsPage(items: [.fixture(id: "item-1")], nextPageToken: "same-page")
            }
            XCTFail("Expected repeated page token error.")
        } catch CacheLibraryPaginationError.repeatedPageToken("same-page") {
            return
        } catch {
            XCTFail("Unexpected error: \(error)")
        }
    }

    func testThrowsWhenServerReturnsTooManyUniquePages() async {
        var requestedPageTokens: [String] = []
        var pageIndex = 0

        do {
            _ = try await collectCacheLibraryItems(maxPages: 3) { pageToken in
                requestedPageTokens.append(pageToken)
                pageIndex += 1
                return CacheLibraryItemsPage(
                    items: [.fixture(id: "item-\(pageIndex)")],
                    nextPageToken: "page-\(pageIndex)"
                )
            }
            XCTFail("Expected page limit error.")
        } catch CacheLibraryPaginationError.exceededPageLimit(3) {
            XCTAssertEqual(requestedPageTokens, ["", "page-1", "page-2"])
        } catch {
            XCTFail("Unexpected error: \(error)")
        }
    }

    func testReturnsPartialResultsWhenAllowedAtPageLimit() async {
        var requestedPageTokens: [String] = []

        do {
            let items = try await collectCacheLibraryItems(
                maxPages: 1,
                allowPartialResults: true
            ) { pageToken in
                requestedPageTokens.append(pageToken)
                return CacheLibraryItemsPage(
                    items: [.fixture(id: "item-1")],
                    nextPageToken: "page-1"
                )
            }

            XCTAssertEqual(items.map(\.id), ["item-1"])
            XCTAssertEqual(requestedPageTokens, [""])
        } catch {
            XCTFail("Unexpected error: \(error)")
        }
    }

    func testThrowsWhenServerReturnsTooManyItems() async {
        do {
            _ = try await collectCacheLibraryItems(maxItems: 2) { _ in
                CacheLibraryItemsPage(
                    items: [
                        .fixture(id: "item-1"),
                        .fixture(id: "item-2"),
                        .fixture(id: "item-3"),
                    ],
                    nextPageToken: ""
                )
            }
            XCTFail("Expected item limit error.")
        } catch CacheLibraryPaginationError.exceededItemLimit(2) {
            return
        } catch {
            XCTFail("Unexpected error: \(error)")
        }
    }
}

extension CacheLibraryItem {
    fileprivate static func fixture(id: String) -> Self {
        Self(
            id: id,
            title: id,
            subtitle: "",
            source: "localCache",
            sourceID: id,
            posterURI: "",
            variants: []
        )
    }
}
