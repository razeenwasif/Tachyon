import SwiftUI
import TachyonCore
import UIKit

/// High-speed run-batched SwiftUI `Canvas` renderer for `TerminalRenderSnapshot`.
struct TerminalCanvasView: View {
    let snapshot: TerminalRenderSnapshot
    let fontSize: Double
    let hostAccent: Color
    let onResize: (Int, Int, Int, Int) -> Void
    let onScroll: (Int) -> Void
    let onFontScale: (Double) -> Void
    let onTapFocus: () -> Void

    @State private var lastCellMetrics: (w: CGFloat, h: CGFloat, font: Double)?
    @State private var dragRemainder: CGFloat = 0
    @State private var basePinchFontSize: Double?

    var body: some View {
        GeometryReader { geo in
            let metrics = measureCell(fontSize: fontSize)
            let cols = max(20, Int(floor((geo.size.width - 8) / max(metrics.w, 4))))
            let rows = max(6, Int(floor((geo.size.height - 6) / max(metrics.h, 8))))

            Canvas(rendersAsynchronously: false) { context, size in
                context.fill(
                    Path(CGRect(origin: .zero, size: size)),
                    with: .color(TachyonPalette.cardBg)
                )
                drawTerminalGrid(
                    context: &context,
                    cellW: metrics.w,
                    cellH: metrics.h,
                    fontSize: fontSize
                )
            }
            .contentShape(Rectangle())
            .onTapGesture {
                onTapFocus()
            }
            .gesture(
                DragGesture(minimumDistance: 10)
                    .onChanged { value in
                        let deltaY = value.translation.height - dragRemainder
                        let lineSteps = Int((deltaY / max(metrics.h, 10)).rounded(.towardZero))
                        if lineSteps != 0 {
                            dragRemainder += CGFloat(lineSteps) * max(metrics.h, 10)
                            onScroll(lineSteps)
                        }
                    }
                    .onEnded { _ in
                        dragRemainder = 0
                    }
            )
            .simultaneousGesture(
                MagnifyGesture()
                    .onChanged { value in
                        if basePinchFontSize == nil {
                            basePinchFontSize = fontSize
                        }
                        if let base = basePinchFontSize {
                            onFontScale(base * value.magnification)
                        }
                    }
                    .onEnded { _ in
                        basePinchFontSize = nil
                    }
            )
            .onAppear {
                onResize(cols, rows, Int(metrics.w), Int(metrics.h))
            }
            .onChange(of: cols) { _, newCols in
                onResize(newCols, rows, Int(metrics.w), Int(metrics.h))
            }
            .onChange(of: rows) { _, newRows in
                onResize(cols, newRows, Int(metrics.w), Int(metrics.h))
            }
        }
    }

    private func measureCell(fontSize: Double) -> (w: CGFloat, h: CGFloat) {
        let font = UIFont.monospacedSystemFont(ofSize: CGFloat(fontSize), weight: .regular)
        let size = ("M" as NSString).size(withAttributes: [.font: font])
        let w = max(4, ceil(size.width))
        let h = max(10, ceil(font.lineHeight))
        return (w, h)
    }

    private func resolvedColors(_ cell: TerminalCell) -> (fg: Color, bg: Color) {
        var (fr, fg, fb) = cell.fg.resolvedRGB(isForeground: true)
        var (br, bg, bb) = cell.bg.resolvedRGB(isForeground: false)
        if cell.attrs.contains(.inverse) {
            (fr, fg, fb, br, bg, bb) = (br, bg, bb, fr, fg, fb)
        }
        let alpha = cell.attrs.contains(.dim) ? 0.58 : 1.0
        return (
            Color(red: fr, green: fg, blue: fb).opacity(alpha),
            Color(red: br, green: bg, blue: bb)
        )
    }

    private func drawTerminalGrid(
        context: inout GraphicsContext,
        cellW: CGFloat,
        cellH: CGFloat,
        fontSize: Double
    ) {
        let padX: CGFloat = 4
        let padY: CGFloat = 3
        let drawRows = min(snapshot.rows, snapshot.cells.count / max(snapshot.cols, 1))
        let drawCols = snapshot.cols

        for r in 0..<drawRows {
            let y = padY + CGFloat(r) * cellH
            var c = 0
            while c < drawCols {
                let cell = snapshot.cell(row: r, col: c)
                if cell.attrs.contains(.wideContinuation) {
                    c += 1
                    continue
                }

                // Wide characters occupy 2 cells and draw individually.
                if cell.attrs.contains(.wide) {
                    let x = padX + CGFloat(c) * cellW
                    let colors = resolvedColors(cell)
                    if cell.bg != .defaultBg || cell.attrs.contains(.inverse) {
                        context.fill(
                            Path(CGRect(x: x, y: y, width: cellW * 2, height: cellH)),
                            with: .color(colors.bg)
                        )
                    }
                    if cell.scalar != " " {
                        let txt = makeStyledText(String(cell.scalar), attrs: cell.attrs, color: colors.fg, fontSize: fontSize)
                        context.draw(txt, at: CGPoint(x: x, y: y), anchor: .topLeading)
                    }
                    c += 2
                    continue
                }

                // Batch contiguous narrow cells with identical style/color/background.
                var runEnd = c + 1
                while runEnd < drawCols {
                    let next = snapshot.cell(row: r, col: runEnd)
                    if next.attrs == cell.attrs && next.fg == cell.fg && next.bg == cell.bg {
                        runEnd += 1
                    } else {
                        break
                    }
                }

                let runCount = runEnd - c
                let x = padX + CGFloat(c) * cellW
                let runW = CGFloat(runCount) * cellW
                let colors = resolvedColors(cell)

                if cell.bg != .defaultBg || cell.attrs.contains(.inverse) {
                    context.fill(
                        Path(CGRect(x: x, y: y, width: runW, height: cellH)),
                        with: .color(colors.bg)
                    )
                }

                var str = ""
                var hasNonSpace = false
                str.reserveCapacity(runCount)
                for colIdx in c..<runEnd {
                    let ch = snapshot.cell(row: r, col: colIdx).scalar
                    if ch != " " { hasNonSpace = true }
                    str.unicodeScalars.append(ch)
                }

                if hasNonSpace {
                    let txt = makeStyledText(str, attrs: cell.attrs, color: colors.fg, fontSize: fontSize)
                    context.draw(txt, at: CGPoint(x: x, y: y), anchor: .topLeading)
                }

                if cell.attrs.contains(.underline) {
                    context.fill(
                        Path(CGRect(x: x, y: y + cellH - 1.5, width: runW, height: 1)),
                        with: .color(colors.fg)
                    )
                }
                if cell.attrs.contains(.strikethrough) {
                    context.fill(
                        Path(CGRect(x: x, y: y + cellH * 0.5, width: runW, height: 1)),
                        with: .color(colors.fg)
                    )
                }

                c = runEnd
            }
        }

        if snapshot.cursorVisible
            && snapshot.cursorRow >= 0 && snapshot.cursorRow < drawRows
            && snapshot.cursorCol >= 0 && snapshot.cursorCol < drawCols {
            let cx = padX + CGFloat(snapshot.cursorCol) * cellW
            let cy = padY + CGFloat(snapshot.cursorRow) * cellH
            let rect = CGRect(x: cx, y: cy, width: cellW, height: cellH)
            context.fill(Path(rect), with: .color(hostAccent.opacity(0.35)))
            context.stroke(Path(rect), with: .color(hostAccent), lineWidth: 1.2)
        }
    }

    private func makeStyledText(
        _ str: String,
        attrs: CellAttributes,
        color: Color,
        fontSize: Double
    ) -> Text {
        var font = Font.system(
            size: CGFloat(fontSize),
            weight: attrs.contains(.bold) ? .bold : .regular,
            design: .monospaced
        )
        if attrs.contains(.italic) {
            font = font.italic()
        }
        return Text(str)
            .font(font)
            .foregroundStyle(color)
    }
}
