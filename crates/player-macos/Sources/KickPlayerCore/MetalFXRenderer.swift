import AppKit
import AVFoundation
import CoreVideo
import Metal
import MetalFX
import QuartzCore

/// Displays decoded `AVPlayer` frames through a MetalFX spatial scaler.
///
/// The renderer owns its own `AVPlayerItemVideoOutput` and a `CAMetalLayer`;
/// the caller hides the regular `AVPlayerLayer` while it is active. Frames are
/// upscaled only when the drawable is larger than the source; otherwise they
/// are blitted with aspect-fit letterboxing.
final class MetalFXRenderer {
    private final class VideoView: NSView {
        let metalLayer = CAMetalLayer()

        override init(frame frameRect: NSRect) {
            super.init(frame: frameRect)
            wantsLayer = true
            layer = metalLayer
            metalLayer.isOpaque = true
            metalLayer.backgroundColor = NSColor.black.cgColor
        }

        required init?(coder: NSCoder) {
            fatalError("init(coder:) has not been implemented")
        }

        override func makeBackingLayer() -> CALayer {
            metalLayer
        }

        override var isOpaque: Bool {
            true
        }

        override func layout() {
            super.layout()
            updateDrawableSize()
        }

        override func viewDidChangeBackingProperties() {
            super.viewDidChangeBackingProperties()
            updateDrawableSize()
        }

        private func updateDrawableSize() {
            let size = convertToBacking(bounds).size
            guard size.width >= 1, size.height >= 1 else { return }
            if metalLayer.drawableSize != size {
                metalLayer.drawableSize = size
            }
        }
    }

    let view: NSView
    private let device: MTLDevice
    private let queue: MTLCommandQueue
    private let metalLayer: CAMetalLayer
    private var textureCache: CVMetalTextureCache?
    private var videoOutput: AVPlayerItemVideoOutput?
    private weak var player: AVPlayer?
    private var displayLink: CVDisplayLink?
    private var scaler: MTLFXSpatialScaler?
    private var scalerInput: (width: Int, height: Int) = (0, 0)
    private var scalerOutput: (width: Int, height: Int) = (0, 0)
    private var intermediate: MTLTexture?
    private var intermediateSize: (width: Int, height: Int) = (0, 0)
    private var running = false
    private(set) weak var outputItem: AVPlayerItem?

    init?() {
        guard let device = MTLCreateSystemDefaultDevice(),
              let queue = device.makeCommandQueue()
        else {
            return nil
        }
        self.device = device
        self.queue = queue
        let videoView = VideoView(frame: .zero)
        self.view = videoView
        self.metalLayer = videoView.metalLayer
        metalLayer.device = device
        metalLayer.pixelFormat = .bgra8Unorm
        metalLayer.framebufferOnly = false
        metalLayer.allowsNextDrawableTimeout = true
        metalLayer.maximumDrawableCount = 3

        guard CVMetalTextureCacheCreate(nil, nil, device, nil, &textureCache) == kCVReturnSuccess else {
            return nil
        }
    }

    /// Attaches a fresh video output to the item and starts the display link.
    func start(player: AVPlayer, item: AVPlayerItem, output: AVPlayerItemVideoOutput) {
        self.player = player
        self.videoOutput = output
        self.outputItem = item
        if !item.outputs.contains(output) {
            item.add(output)
        }
        view.frame = view.superview?.bounds ?? view.frame
        view.autoresizingMask = [.width, .height]
        startDisplayLink()
    }

    /// Stops rendering and detaches the output from the item.
    func stop(item: AVPlayerItem?) {
        stopDisplayLink()
        if let output = videoOutput, let item, item.outputs.contains(output) {
            item.remove(output)
        }
        videoOutput = nil
        player = nil
        outputItem = nil
        view.removeFromSuperview()
    }

    var isRunning: Bool {
        running
    }

    private func startDisplayLink() {
        guard displayLink == nil else {
            if !running {
                CVDisplayLinkStart(displayLink!)
                running = true
            }
            return
        }
        var link: CVDisplayLink?
        CVDisplayLinkCreateWithActiveCGDisplays(&link)
        guard let link else { return }
        CVDisplayLinkSetOutputHandler(link) { [weak self] _, _, _, _, _ in
            self?.renderFrame()
            return kCVReturnSuccess
        }
        CVDisplayLinkStart(link)
        displayLink = link
        running = true
    }

    private func stopDisplayLink() {
        if let link = displayLink, running {
            CVDisplayLinkStop(link)
        }
        running = false
    }

    deinit {
        if let link = displayLink {
            CVDisplayLinkStop(link)
        }
    }

    private func renderFrame() {
        guard running,
              let player,
              let output = videoOutput,
              let cache = textureCache
        else {
            return
        }
        let time = player.currentTime()
        guard output.hasNewPixelBuffer(forItemTime: time),
              let pixelBuffer = output.copyPixelBuffer(forItemTime: time, itemTimeForDisplay: nil)
        else {
            return
        }
        let width = CVPixelBufferGetWidth(pixelBuffer)
        let height = CVPixelBufferGetHeight(pixelBuffer)
        guard width > 1, height > 1 else { return }

        var cvTexture: CVMetalTexture?
        let status = CVMetalTextureCacheCreateTextureFromImage(
            nil,
            cache,
            pixelBuffer,
            nil,
            .bgra8Unorm,
            width,
            height,
            0,
            &cvTexture
        )
        guard status == kCVReturnSuccess,
              let cvTexture,
              let input = CVMetalTextureGetTexture(cvTexture)
        else {
            return
        }

        let drawableSize = metalLayer.drawableSize
        guard drawableSize.width > 1, drawableSize.height > 1,
              let drawable = metalLayer.nextDrawable()
        else {
            return
        }

        let scale = min(
            drawableSize.width / CGFloat(width),
            drawableSize.height / CGFloat(height)
        )
        let outWidth = max(1, Int((CGFloat(width) * scale).rounded(.down)))
        let outHeight = max(1, Int((CGFloat(height) * scale).rounded(.down)))
        guard outWidth > 0, outHeight > 0 else { return }

        let commandBuffer = queue.makeCommandBuffer()
        guard let commandBuffer else { return }

        // Clear the drawable to black so letterbox bars stay black.
        let pass = MTLRenderPassDescriptor()
        pass.colorAttachments[0].texture = drawable.texture
        pass.colorAttachments[0].loadAction = .clear
        pass.colorAttachments[0].storeAction = .store
        pass.colorAttachments[0].clearColor = MTLClearColorMake(0, 0, 0, 1)
        commandBuffer.makeRenderCommandEncoder(descriptor: pass)?.endEncoding()

        let destinationX = (Int(drawableSize.width) - outWidth) / 2
        let destinationY = (Int(drawableSize.height) - outHeight) / 2

        if outWidth > width, let scaled = upscale(
            input: input,
            inputSize: (width, height),
            outputSize: (outWidth, outHeight),
            commandBuffer: commandBuffer
        ) {
            blit(from: scaled, to: drawable.texture, commandBuffer: commandBuffer,
                  destination: (destinationX, destinationY))
        } else {
            blit(from: input, to: drawable.texture, commandBuffer: commandBuffer,
                  destination: (destinationX, destinationY), sourceSize: (outWidth, outHeight))
        }

        commandBuffer.present(drawable)
        commandBuffer.commit()
    }

    private func upscale(
        input: MTLTexture,
        inputSize: (width: Int, height: Int),
        outputSize: (width: Int, height: Int),
        commandBuffer: MTLCommandBuffer
    ) -> MTLTexture? {
        if scaler == nil || scalerInput != inputSize || scalerOutput != outputSize {
            let descriptor = MTLFXSpatialScalerDescriptor()
            descriptor.inputWidth = inputSize.width
            descriptor.inputHeight = inputSize.height
            descriptor.outputWidth = outputSize.width
            descriptor.outputHeight = outputSize.height
            descriptor.colorTextureFormat = .bgra8Unorm
            descriptor.outputTextureFormat = .bgra8Unorm
            descriptor.colorProcessingMode = .perceptual
            guard let newScaler = descriptor.makeSpatialScaler(device: device) else {
                return nil
            }
            scaler = newScaler
            scalerInput = inputSize
            scalerOutput = outputSize
        }
        if intermediate == nil || intermediateSize != outputSize {
            let descriptor = MTLTextureDescriptor.texture2DDescriptor(
                pixelFormat: .bgra8Unorm,
                width: outputSize.width,
                height: outputSize.height,
                mipmapped: false
            )
            descriptor.usage = [.shaderRead, .renderTarget]
            descriptor.storageMode = .private
            intermediate = device.makeTexture(descriptor: descriptor)
            intermediateSize = outputSize
        }
        guard let scaler, let intermediate else { return nil }
        scaler.colorTexture = input
        scaler.outputTexture = intermediate
        scaler.inputContentWidth = inputSize.width
        scaler.inputContentHeight = inputSize.height
        scaler.encode(commandBuffer: commandBuffer)
        return intermediate
    }

    private func blit(
        from source: MTLTexture,
        to destination: MTLTexture,
        commandBuffer: MTLCommandBuffer,
        destination origin: (x: Int, y: Int),
        sourceSize: (width: Int, height: Int)? = nil
    ) {
        let size = sourceSize ?? (width: source.width, height: source.height)
        guard let encoder = commandBuffer.makeBlitCommandEncoder() else { return }
        encoder.copy(
            from: source,
            sourceSlice: 0,
            sourceLevel: 0,
            sourceOrigin: MTLOrigin(x: 0, y: 0, z: 0),
            sourceSize: MTLSize(width: size.width, height: size.height, depth: 1),
            to: destination,
            destinationSlice: 0,
            destinationLevel: 0,
            destinationOrigin: MTLOrigin(x: origin.x, y: origin.y, z: 0)
        )
        encoder.endEncoding()
    }
}

/// Creates the pixel-buffer output used by the Metal renderer.
func makeMetalVideoOutput() -> AVPlayerItemVideoOutput {
    let attributes: [String: Any] = [
        kCVPixelBufferPixelFormatTypeKey as String: kCVPixelFormatType_32BGRA,
        kCVPixelBufferMetalCompatibilityKey as String: true,
    ]
    return AVPlayerItemVideoOutput(pixelBufferAttributes: attributes)
}
