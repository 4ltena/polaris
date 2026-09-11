import Foundation
import XCTest
@testable import PolarisDesktop

final class WorkspaceComposerInputTests: XCTestCase {
    func testAttachmentPreviewUsesOnlyCallerSuppliedDisplayValues() {
        let attachment = WorkspaceAttachment(url: URL(fileURLWithPath: "/not-imported/private.txt"))
        let preview = WorkspaceComposerAttachmentPreview(attachment: attachment,
                                                         detail: "UTF-8 text · 14 bytes",
                                                         text: "確認済みの本文")

        XCTAssertEqual(preview.id, attachment.id)
        XCTAssertEqual(preview.name, "private.txt")
        XCTAssertEqual(preview.detail, "UTF-8 text · 14 bytes")
        XCTAssertEqual(preview.text, "確認済みの本文")
    }
}
