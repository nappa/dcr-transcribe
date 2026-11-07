import XCTest
import CoreImage
@testable import DCRMonitor

final class MobileShareTests: XCTestCase {
    func testQRCodeDecodesToCompleteLoginURL() throws {
        let url = "https://example.com/?k=0123456789abcdef0123456789abcdef"
        let image = try XCTUnwrap(MobileShareView.qrImage(url))
        let data = try XCTUnwrap(image.tiffRepresentation)
        let ciImage = try XCTUnwrap(CIImage(data: data))
        let detector = try XCTUnwrap(CIDetector(ofType: CIDetectorTypeQRCode, context: CIContext(),
                                               options: [CIDetectorAccuracy: CIDetectorAccuracyHigh]))
        let result = try XCTUnwrap(detector.features(in: ciImage).first as? CIQRCodeFeature)
        XCTAssertEqual(result.messageString, url)
    }
}
