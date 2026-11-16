import Foundation
import AVFoundation
import QuartzCore
@main struct Probe {
 @MainActor static func main() {
  guard Bundle.main.bundleIdentifier == "dev.codex.kick-direct-check-20261112", let input = readLine(), let url = URL(string:input), url.scheme == "https" else { print("invalid input or bundle identity"); exit(2) }
  guard (HTTPCookieStorage.shared.cookies ?? []).isEmpty else { print("probe_cookie_store_not_empty"); exit(2) }
  HTTPCookieStorage.shared.cookieAcceptPolicy = .never
  let player = AVPlayer(url:url)
  let layer = AVPlayerLayer(player:player)
  player.play()
  var baseline: Double? = nil
  var advanced = false
  let deadline=Date().addingTimeInterval(20)
  while Date() < deadline {
   RunLoop.main.run(until:Date().addingTimeInterval(0.25))
   if player.currentItem?.status == .failed { break }
   let time = player.currentTime()
   if player.currentItem?.status == .readyToPlay && time.isNumeric && time.seconds.isFinite {
    if baseline == nil { baseline = time.seconds }
    if let start = baseline, time.seconds - start >= 3 && player.rate > 0 { advanced = true; break }
   }
  }
  print("item_ready",player.currentItem?.status == .readyToPlay)
  print("transport_playing",player.timeControlStatus == .playing)
  print("time_advanced_3s",advanced)
  print("layer_ready",layer.isReadyForDisplay)
  if let error=player.currentItem?.error as NSError? { print("error_domain",error.domain,"error_code",error.code) }
  let success=advanced && player.currentItem?.status == .readyToPlay && player.timeControlStatus == .playing && layer.isReadyForDisplay
  player.pause(); player.replaceCurrentItem(with:nil); layer.player=nil
  print("item_released",player.currentItem == nil)
  exit(success ? 0:1)
 }
}
