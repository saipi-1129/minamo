import AppKit
import QuartzCore

/// Draws scrolling chat comments over the native player view.
///
/// The AVPlayer view is an AppKit subview, so GPUI content cannot be painted
/// above it. This transparent overlay is added as a subview of the player view
/// and animates `CATextLayer`s from right to left.
final class DanmakuOverlayView: NSView {
    private struct ActiveComment {
        let layer: CATextLayer
        let expiresAt: CFTimeInterval
    }

    private var active: [ActiveComment] = []
    private var laneFreeAt: [CFTimeInterval] = []
    private var fontSize: CGFloat = 20
    private var opacity: CGFloat = 0.9

    private static let laneHeight: CGFloat = 26
    private static let maxLanes = 16
    private static let travelSeconds: CFTimeInterval = 8

    override var isOpaque: Bool { false }
    override var isFlipped: Bool { true }

    func configure(fontSize: CGFloat, opacity: CGFloat) {
        self.fontSize = max(10, min(48, fontSize))
        self.opacity = max(0.2, min(1, opacity))
    }

    func push(_ text: String) {
        let trimmed = text.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !trimmed.isEmpty else { return }
        guard bounds.width > 1, bounds.height > 1 else { return }
        pruneExpired()

        let attributes: [NSAttributedString.Key: Any] = [
            .font: NSFont.systemFont(ofSize: fontSize, weight: .semibold),
        ]
        let textSize = (trimmed as NSString).size(withAttributes: attributes)
        let width = min(bounds.width, textSize.width + 16)
        let now = CACurrentMediaTime()
        guard let lane = availableLane(now: now) else { return }
        let speed = (bounds.width + width) / CGFloat(Self.travelSeconds)
        laneFreeAt[lane] = now + Double((width + 48) / speed)
        let y = min(
            CGFloat(lane) * Self.laneHeight,
            max(0, bounds.height - Self.laneHeight)
        )

        let layer = CATextLayer()
        layer.string = trimmed
        layer.font = NSFont.systemFont(ofSize: fontSize, weight: .semibold)
        layer.fontSize = fontSize
        layer.foregroundColor = NSColor.white.withAlphaComponent(opacity).cgColor
        layer.shadowColor = NSColor.black.cgColor
        layer.shadowOpacity = 0.75
        layer.shadowRadius = 2
        layer.shadowOffset = .zero
        layer.alignmentMode = .left
        layer.truncationMode = .end
        layer.isWrapped = false
        layer.contentsScale = window?.backingScaleFactor ?? 2
        layer.frame = CGRect(x: bounds.width, y: y, width: width, height: Self.laneHeight)

        wantsLayer = true
        self.layer?.addSublayer(layer)

        let endX = -width
        let animation = CABasicAnimation(keyPath: "position.x")
        animation.fromValue = bounds.width + width / 2
        animation.toValue = endX + width / 2
        animation.duration = Self.travelSeconds
        animation.timingFunction = CAMediaTimingFunction(name: .linear)
        animation.isRemovedOnCompletion = true
        layer.position = CGPoint(x: endX + width / 2, y: y + Self.laneHeight / 2)
        layer.add(animation, forKey: "danmaku-move")

        active.append(
            ActiveComment(layer: layer, expiresAt: CACurrentMediaTime() + Self.travelSeconds + 0.5)
        )
    }

    func clear() {
        for comment in active {
            comment.layer.removeFromSuperlayer()
        }
        active.removeAll()
    }

    /// Number of lanes that fit in the current video height. Recomputed on
    /// every push so the comments use the whole surface, not just the top.
    private func laneCount() -> Int {
        guard bounds.height > 1 else { return 1 }
        return max(1, min(Self.maxLanes, Int(bounds.height / Self.laneHeight)))
    }

    /// Returns a lane whose previous comment has fully entered the screen.
    /// Returns nil when every lane is still busy so comments never overlap.
    private func availableLane(now: CFTimeInterval) -> Int? {
        let count = laneCount()
        if laneFreeAt.count != count {
            laneFreeAt = Array(repeating: 0, count: count)
        }
        return laneFreeAt.firstIndex { now >= $0 }
    }

    private func pruneExpired() {
        let now = CACurrentMediaTime()
        active.removeAll { comment in
            if comment.expiresAt <= now {
                comment.layer.removeFromSuperlayer()
                return true
            }
            return false
        }
    }
}
