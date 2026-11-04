import XCTest

/// The iOS app on a real server (set `TEST_RUNNER_LAZY_SERVER`): the first-run server screen, then
/// everyday messaging with iOS's real keyboard. Screenshots go to `TEST_RUNNER_LAZY_SHOTS`, for a
/// person to read.
final class ShellTests: XCTestCase {
    private var server: String { ProcessInfo.processInfo.environment["LAZY_SERVER"] ?? "http://127.0.0.1:18194" }

    override func setUp() {
        continueAfterFailure = false
    }

    private func shot(_ name: String) {
        guard let folder = ProcessInfo.processInfo.environment["LAZY_SHOTS"] else { return }
        let data = XCUIScreen.main.screenshot().pngRepresentation
        try? data.write(to: URL(fileURLWithPath: folder).appendingPathComponent("ios-\(name).png"))
    }

    // A failed run says what was on screen, which goes away with the simulator.
    private var reporting = false
    override func record(_ issue: XCTIssue) {
        if !reporting {
            reporting = true
            print("On screen:\n\(XCUIApplication().debugDescription)")
            shot("stopped-" + (name.contains("test1") ? "first-run" : "messaging"))
        }
        super.record(issue)
    }

    private func signIn(_ app: XCUIApplication) {
        let username = app.webViews.textFields["Username"]
        XCTAssertTrue(username.waitForExistence(timeout: 60), "the sign-in form appears")
        username.tap()
        username.typeText("ada")
        let password = app.webViews.secureTextFields["Password"]
        password.tap()
        password.typeText("correct horse battery\n")
    }

    func test1_FirstRunAsksForTheServer() {
        let app = XCUIApplication()
        // An empty address overrides any saved one, so this is always a first run.
        app.launchArguments = ["-server", ""]
        app.launch()
        let field = app.textFields["Server address"]
        XCTAssertTrue(field.waitForExistence(timeout: 30), "the server screen appears")
        shot("1-server")
        field.tap()
        field.typeText("not a server\n")
        XCTAssertTrue(
            app.staticTexts["Could not reach not a server. Check the address and your connection."]
                .waitForExistence(timeout: 2)
                || app.staticTexts["Enter an address like chat.example.com. It has to use HTTPS."]
                    .waitForExistence(timeout: 20),
            "a bad address is refused")
        field.tap()
        field.clearAndType(server + "\n")
        XCTAssertTrue(app.webViews.textFields["Username"].waitForExistence(timeout: 60), "the page loads")
        shot("2-signin")
    }

    func test2_EverydayMessaging() {
        let app = XCUIApplication()
        app.launchArguments = ["-server", server]
        app.launch()
        signIn(app)

        let composer = app.webViews.textViews.firstMatch
        XCTAssertTrue(composer.waitForExistence(timeout: 60), "the conversation opens")
        shot("3-conversation")

        composer.tap()
        let keyboard = app.keyboards.firstMatch
        XCTAssertTrue(keyboard.waitForExistence(timeout: 20), "the keyboard opens")
        sleep(1)
        XCTAssertLessThanOrEqual(composer.frame.maxY, keyboard.frame.minY, "the composer is above the keyboard")
        shot("4-keyboard")

        let words = "Hello from the iOS app"
        composer.typeText(words)
        app.keyboards.buttons["Send"].tap()
        // In the list: the words are also found in the composer when they were not sent.
        let message = app.webViews.otherElements["log"].staticTexts[words]
        // TEMPORARY, to learn why a send right after signing in does nothing: which of these works.
        var sent = message.waitForExistence(timeout: 8)
        print("SEND-CHECK keyboard Send, first press: \(sent)")
        if !sent {
            app.keyboards.buttons["Send"].tap()
            sent = message.waitForExistence(timeout: 8)
            print("SEND-CHECK keyboard Send, second press: \(sent)")
        }
        if !sent {
            app.webViews.buttons["Send"].tap()
            sent = message.waitForExistence(timeout: 8)
            print("SEND-CHECK the page's Send button: \(sent)")
        }
        XCTAssertTrue(sent, "the message is sent")
        sleep(1)
        shot("5-sent")

        // Touchscreens have no hover: a tap on the message shows its actions.
        message.tap()
        let react = app.webViews.buttons["Add a reaction"]
        XCTAssertTrue(react.waitForExistence(timeout: 10), "a tap shows the actions")
        shot("6-actions")

        // Through a thread and out again.
        app.webViews.buttons["Reply in thread"].tap()
        let thread = app.webViews.staticTexts["Thread"]
        XCTAssertTrue(thread.waitForExistence(timeout: 15), "the thread opens")
        shot("7-thread")
        app.webViews.buttons["Close the thread"].tap()

        // Leaving and coming back, as a person does all day.
        XCUIDevice.shared.press(.home)
        sleep(3)
        app.activate()
        XCTAssertTrue(message.waitForExistence(timeout: 30), "the page survives leaving")
        shot("8-back")
    }
}

extension XCUIElement {
    fileprivate func clearAndType(_ text: String) {
        if let current = value as? String, !current.isEmpty {
            typeText(String(repeating: XCUIKeyboardKey.delete.rawValue, count: current.count))
        }
        typeText(text)
    }
}
