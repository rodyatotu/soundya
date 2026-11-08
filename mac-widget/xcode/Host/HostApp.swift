// Only a container for building the widget with Xcode (xcodebuild -target SoundyaWidget):
// scripts/mac-widget/build.sh takes SoundyaWidget.appex out of the build and puts it into soundya.app.
import SwiftUI

@main
struct HostApp: App {
    var body: some Scene { WindowGroup { Text("soundya widget host") } }
}
