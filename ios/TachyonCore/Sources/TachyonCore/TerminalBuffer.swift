import Foundation

/// Terminal cell foreground/background color.
public enum TerminalColor: Sendable, Equatable {
    case defaultFg
    case defaultBg
    case indexed(UInt8)
    case rgb(UInt8, UInt8, UInt8)

    /// Resolve to Obsidian minimal RGB components (`0...1`).
    public func resolvedRGB(isForeground: Bool) -> (Double, Double, Double) {
        switch self {
        case .defaultFg:
            return (0.902, 0.929, 0.961) // #E6EDF5
        case .defaultBg:
            return (0.067, 0.071, 0.078) // #111214
        case .rgb(let r, let g, let b):
            return (Double(r) / 255.0, Double(g) / 255.0, Double(b) / 255.0)
        case .indexed(let idx):
            return Self.xtermColor(idx, isForeground: isForeground)
        }
    }

    private static let ansi16: [(Double, Double, Double)] = [
        (0.110, 0.118, 0.141), // 0 black
        (0.878, 0.424, 0.459), // 1 red
        (0.514, 0.788, 0.569), // 2 green
        (0.902, 0.745, 0.424), // 3 yellow
        (0.588, 0.698, 0.843), // 4 blue (#96B2D7)
        (0.761, 0.588, 0.855), // 5 magenta
        (0.455, 0.788, 0.780), // 6 cyan
        (0.820, 0.847, 0.882), // 7 white
        (0.388, 0.424, 0.478), // 8 bright black
        (0.945, 0.506, 0.541), // 9 bright red
        (0.608, 0.855, 0.659), // 10 bright green
        (0.961, 0.824, 0.514), // 11 bright yellow
        (0.686, 0.788, 0.918), // 12 bright blue
        (0.843, 0.698, 0.925), // 13 bright magenta
        (0.569, 0.863, 0.855), // 14 bright cyan
        (0.953, 0.965, 0.980), // 15 bright white
    ]

    private static func xtermColor(_ idx: UInt8, isForeground: Bool) -> (Double, Double, Double) {
        if idx < 16 {
            return ansi16[Int(idx)]
        }
        if idx < 232 {
            let v = Int(idx) - 16
            let b = v % 6
            let g = (v / 6) % 6
            let r = v / 36
            let map: [Double] = [0.0, 95.0 / 255.0, 135.0 / 255.0, 175.0 / 255.0, 215.0 / 255.0, 1.0]
            return (map[r], map[g], map[b])
        }
        let level = Double(8 + (Int(idx) - 232) * 10) / 255.0
        let clamped = min(max(level, 0.0), 1.0)
        return isForeground ? (clamped, clamped, clamped) : (clamped, clamped, clamped)
    }
}

public struct CellAttributes: OptionSet, Sendable, Equatable {
    public let rawValue: UInt16
    public init(rawValue: UInt16) { self.rawValue = rawValue }

    public static let bold = CellAttributes(rawValue: 1 << 0)
    public static let dim = CellAttributes(rawValue: 1 << 1)
    public static let italic = CellAttributes(rawValue: 1 << 2)
    public static let underline = CellAttributes(rawValue: 1 << 3)
    public static let inverse = CellAttributes(rawValue: 1 << 4)
    public static let strikethrough = CellAttributes(rawValue: 1 << 5)
    public static let wide = CellAttributes(rawValue: 1 << 6)
    public static let wideContinuation = CellAttributes(rawValue: 1 << 7)
}

public struct TerminalCell: Sendable, Equatable {
    public var scalar: UnicodeScalar
    public var fg: TerminalColor
    public var bg: TerminalColor
    public var attrs: CellAttributes

    public static let blank = TerminalCell(
        scalar: " ",
        fg: .defaultFg,
        bg: .defaultBg,
        attrs: []
    )

    public init(
        scalar: UnicodeScalar = " ",
        fg: TerminalColor = .defaultFg,
        bg: TerminalColor = .defaultBg,
        attrs: CellAttributes = []
    ) {
        self.scalar = scalar
        self.fg = fg
        self.bg = bg
        self.attrs = attrs
    }
}

/// Immutable snapshot of visible terminal rows for zero-tearing SwiftUI rendering.
public struct TerminalRenderSnapshot: Sendable, Equatable {
    public let cols: Int
    public let rows: Int
    public let cells: [TerminalCell]
    public let cursorRow: Int
    public let cursorCol: Int
    public let cursorVisible: Bool
    public let scrollOffset: Int
    public let scrollbackCount: Int
    public let title: String
    public let cwd: String
    public let agentStatus: ProgramStatus?
    public let applicationCursorKeys: Bool
    public let bracketedPasteMode: Bool
    public let alternateScreen: Bool

    public func cell(row: Int, col: Int) -> TerminalCell {
        guard row >= 0, row < rows, col >= 0, col < cols else { return .blank }
        return cells[row * cols + col]
    }
}

/// Lightweight VT100/ANSI/xterm terminal state machine for iOS Mux v2 streams.
public final class TerminalBuffer {
    public private(set) var cols: Int
    public private(set) var rows: Int
    public private(set) var cursorRow: Int = 0
    public private(set) var cursorCol: Int = 0
    public private(set) var cursorVisible: Bool = true
    public private(set) var applicationCursorKeys: Bool = false
    public private(set) var bracketedPasteMode: Bool = false
    public private(set) var inAltScreen: Bool = false
    public private(set) var title: String = ""
    public private(set) var cwd: String = ""
    public private(set) var agentStatus: ProgramStatus?

    private var primaryGrid: [[TerminalCell]]
    private var altGrid: [[TerminalCell]]
    private var scrollback: [[TerminalCell]] = []
    private let maxScrollback: Int

    private var primarySavedCursor = (row: 0, col: 0, fg: TerminalColor.defaultFg, bg: TerminalColor.defaultBg, attrs: CellAttributes())
    private var altSavedCursor = (row: 0, col: 0, fg: TerminalColor.defaultFg, bg: TerminalColor.defaultBg, attrs: CellAttributes())

    private var currentFg: TerminalColor = .defaultFg
    private var currentBg: TerminalColor = .defaultBg
    private var currentAttrs: CellAttributes = []

    private var scrollTop: Int = 0
    private var scrollBottom: Int
    private var pendingWrap: Bool = false

    private enum ParseState {
        case ground
        case escape
        case csi
        case osc
        case oscEscape
        case stringConsume // DCS, APC, PM, SOS ignored until ST/BEL
        case stringEscape
        case charset
    }

    private var parseState: ParseState = .ground
    private var csiBytes: [UInt8] = []
    private var oscBytes: [UInt8] = []
    private var utf8Pending: [UInt8] = []
    private var utf8Expected: Int = 0

    public init(cols: Int = 80, rows: Int = 24, maxScrollback: Int = 2000) {
        let c = max(1, cols)
        let r = max(1, rows)
        self.cols = c
        self.rows = r
        self.maxScrollback = max(0, maxScrollback)
        self.primaryGrid = Self.makeGrid(cols: c, rows: r)
        self.altGrid = Self.makeGrid(cols: c, rows: r)
        self.scrollBottom = r - 1
    }

    public func reset() {
        primaryGrid = Self.makeGrid(cols: cols, rows: rows)
        altGrid = Self.makeGrid(cols: cols, rows: rows)
        scrollback.removeAll(keepingCapacity: true)
        inAltScreen = false
        cursorRow = 0
        cursorCol = 0
        cursorVisible = true
        applicationCursorKeys = false
        bracketedPasteMode = false
        currentFg = .defaultFg
        currentBg = .defaultBg
        currentAttrs = []
        scrollTop = 0
        scrollBottom = rows - 1
        pendingWrap = false
        parseState = .ground
        csiBytes.removeAll(keepingCapacity: true)
        oscBytes.removeAll(keepingCapacity: true)
        utf8Pending.removeAll(keepingCapacity: true)
        utf8Expected = 0
        title = ""
        cwd = ""
        agentStatus = nil
    }

    public func resize(cols newCols: Int, rows newRows: Int) {
        let nc = max(1, newCols)
        let nr = max(1, newRows)
        guard nc != cols || nr != rows else { return }

        primaryGrid = Self.resizedGrid(primaryGrid, oldCols: cols, newCols: nc, newRows: nr)
        altGrid = Self.resizedGrid(altGrid, oldCols: cols, newCols: nc, newRows: nr)
        cols = nc
        rows = nr
        scrollTop = 0
        scrollBottom = nr - 1
        cursorRow = min(max(cursorRow, 0), nr - 1)
        cursorCol = min(max(cursorCol, 0), nc - 1)
        pendingWrap = false
    }

    public var scrollbackCount: Int {
        inAltScreen ? 0 : scrollback.count
    }

    public func feed(_ data: Data) {
        for byte in data {
            processByte(byte)
        }
    }

    public func plainText(lastLines: Int? = nil) -> String {
        let sourceRows: [[TerminalCell]]
        if inAltScreen {
            sourceRows = altGrid
        } else {
            sourceRows = scrollback + primaryGrid
        }
        let slice = lastLines.map { Array(sourceRows.suffix(max(0, $0))) } ?? sourceRows
        return slice.map { line in
            var s = ""
            for cell in line where !cell.attrs.contains(.wideContinuation) {
                s.unicodeScalars.append(cell.scalar)
            }
            while s.last == " " { s.removeLast() }
            return s
        }.joined(separator: "\n")
    }

    public func renderSnapshot(scrollOffset: Int = 0) -> TerminalRenderSnapshot {
        let clampedOffset = inAltScreen ? 0 : min(max(0, scrollOffset), scrollback.count)
        var flat: [TerminalCell] = []
        flat.reserveCapacity(rows * cols)

        if inAltScreen {
            for r in 0..<rows {
                flat.append(contentsOf: altGrid[r])
            }
        } else if clampedOffset == 0 {
            for r in 0..<rows {
                flat.append(contentsOf: primaryGrid[r])
            }
        } else {
            let start = scrollback.count - clampedOffset
            for i in 0..<rows {
                let idx = start + i
                let rowCells: [TerminalCell]
                if idx < scrollback.count {
                    rowCells = scrollback[idx]
                } else {
                    rowCells = primaryGrid[idx - scrollback.count]
                }
                if rowCells.count >= cols {
                    flat.append(contentsOf: rowCells[0..<cols])
                } else {
                    flat.append(contentsOf: rowCells)
                    flat.append(contentsOf: repeatElement(TerminalCell.blank, count: cols - rowCells.count))
                }
            }
        }

        return TerminalRenderSnapshot(
            cols: cols,
            rows: rows,
            cells: flat,
            cursorRow: min(max(0, cursorRow), rows - 1),
            cursorCol: min(max(0, cursorCol), cols - 1),
            cursorVisible: cursorVisible && clampedOffset == 0,
            scrollOffset: clampedOffset,
            scrollbackCount: inAltScreen ? 0 : scrollback.count,
            title: title,
            cwd: cwd,
            agentStatus: agentStatus,
            applicationCursorKeys: applicationCursorKeys,
            bracketedPasteMode: bracketedPasteMode,
            alternateScreen: inAltScreen
        )
    }

    // MARK: - Byte & Escape Parsing

    private func processByte(_ b: UInt8) {
        switch parseState {
        case .ground:
            if utf8Expected > 0 {
                if (b & 0xC0) == 0x80 {
                    utf8Pending.append(b)
                    if utf8Pending.count == utf8Expected {
                        let bytes = utf8Pending
                        utf8Pending.removeAll(keepingCapacity: true)
                        utf8Expected = 0
                        if let str = String(bytes: bytes, encoding: .utf8),
                           let scalar = str.unicodeScalars.first {
                            putScalar(scalar)
                        } else {
                            putScalar("\u{FFFD}")
                        }
                    }
                    return
                } else {
                    utf8Pending.removeAll(keepingCapacity: true)
                    utf8Expected = 0
                    putScalar("\u{FFFD}")
                    // Fall through to interpret current byte cleanly.
                }
            }

            if b < 0x20 || b == 0x7F {
                handleControl(b)
            } else if b < 0x80 {
                putScalar(UnicodeScalar(b))
            } else if (b & 0xE0) == 0xC0 {
                utf8Pending = [b]
                utf8Expected = 2
            } else if (b & 0xF0) == 0xE0 {
                utf8Pending = [b]
                utf8Expected = 3
            } else if (b & 0xF8) == 0xF0 {
                utf8Pending = [b]
                utf8Expected = 4
            } else {
                putScalar("\u{FFFD}")
            }

        case .escape:
            handleEscapeByte(b)

        case .csi:
            if b == 0x18 || b == 0x1A {
                parseState = .ground
                csiBytes.removeAll(keepingCapacity: true)
            } else if b == 0x1B {
                csiBytes.removeAll(keepingCapacity: true)
                parseState = .escape
            } else if b < 0x20 {
                handleControl(b)
            } else if (0x40...0x7E).contains(b) {
                executeCSI(final: b)
                csiBytes.removeAll(keepingCapacity: true)
                parseState = .ground
            } else {
                if csiBytes.count < 256 {
                    csiBytes.append(b)
                }
            }

        case .osc:
            if b == 0x07 {
                executeOSC()
                oscBytes.removeAll(keepingCapacity: true)
                parseState = .ground
            } else if b == 0x1B {
                parseState = .oscEscape
            } else if b == 0x18 || b == 0x1A {
                oscBytes.removeAll(keepingCapacity: true)
                parseState = .ground
            } else if oscBytes.count < 65_536 {
                oscBytes.append(b)
            }

        case .oscEscape:
            if b == 0x5C /* '\\' */ {
                executeOSC()
                oscBytes.removeAll(keepingCapacity: true)
                parseState = .ground
            } else if b == 0x1B {
                break
            } else {
                oscBytes.removeAll(keepingCapacity: true)
                parseState = .ground
                handleEscapeByte(b)
            }

        case .stringConsume:
            if b == 0x1B {
                parseState = .stringEscape
            } else if b == 0x07 {
                parseState = .ground
            }

        case .stringEscape:
            if b == 0x5C {
                parseState = .ground
            } else if b != 0x1B {
                parseState = .stringConsume
            }

        case .charset:
            parseState = .ground
        }
    }

    private func handleControl(_ b: UInt8) {
        switch b {
        case 0x07: // BEL
            break
        case 0x08: // BS
            pendingWrap = false
            if cursorCol > 0 { cursorCol -= 1 }
        case 0x09: // HT
            pendingWrap = false
            let nextTab = min(cols - 1, ((cursorCol / 8) + 1) * 8)
            cursorCol = nextTab
        case 0x0A, 0x0B, 0x0C: // LF, VT, FF
            lineFeed()
        case 0x0D: // CR
            pendingWrap = false
            cursorCol = 0
        case 0x1B: // ESC
            parseState = .escape
        default:
            break
        }
    }

    private func handleEscapeByte(_ b: UInt8) {
        switch b {
        case 0x5B: // '[' -> CSI
            csiBytes.removeAll(keepingCapacity: true)
            parseState = .csi
        case 0x5D: // ']' -> OSC
            oscBytes.removeAll(keepingCapacity: true)
            parseState = .osc
        case 0x50, 0x5F, 0x5E, 0x58: // DCS 'P', APC '_', PM '^', SOS 'X'
            parseState = .stringConsume
        case 0x28, 0x29, 0x2A, 0x2B: // G0..G3 charset designation
            parseState = .charset
        case 0x37: // '7' DECSC
            saveCursor()
            parseState = .ground
        case 0x38: // '8' DECRC
            restoreCursor()
            parseState = .ground
        case 0x44: // 'D' IND
            lineFeed()
            parseState = .ground
        case 0x45: // 'E' NEL
            cursorCol = 0
            lineFeed()
            parseState = .ground
        case 0x4D: // 'M' RI
            reverseIndex()
            parseState = .ground
        case 0x63: // 'c' RIS
            reset()
            parseState = .ground
        default:
            parseState = .ground
        }
    }

    // MARK: - Grid Manipulation

    private func currentRowSlice(_ r: Int, set cells: [TerminalCell]) {
        if inAltScreen {
            altGrid[r] = cells
        } else {
            primaryGrid[r] = cells
        }
    }

    private func activeGridRead(_ r: Int) -> [TerminalCell] {
        inAltScreen ? altGrid[r] : primaryGrid[r]
    }

    private func writeCell(row r: Int, col c: Int, cell: TerminalCell) {
        guard r >= 0, r < rows, c >= 0, c < cols else { return }
        if inAltScreen {
            altGrid[r][c] = cell
        } else {
            primaryGrid[r][c] = cell
        }
    }

    private func putScalar(_ scalar: UnicodeScalar) {
        let width = Self.displayWidth(scalar)
        if width == 0 {
            return
        }
        if pendingWrap {
            cursorCol = 0
            lineFeed()
            pendingWrap = false
        }

        if width == 2 && cols >= 2 && cursorCol == cols - 1 {
            writeCell(row: cursorRow, col: cursorCol, cell: .blank)
            cursorCol = 0
            lineFeed()
        }

        var attrs = currentAttrs
        if width == 2 && cursorCol + 1 < cols {
            attrs.insert(.wide)
            writeCell(
                row: cursorRow,
                col: cursorCol,
                cell: TerminalCell(scalar: scalar, fg: currentFg, bg: currentBg, attrs: attrs)
            )
            var contAttrs = currentAttrs
            contAttrs.insert(.wideContinuation)
            writeCell(
                row: cursorRow,
                col: cursorCol + 1,
                cell: TerminalCell(scalar: " ", fg: currentFg, bg: currentBg, attrs: contAttrs)
            )
            if cursorCol + 2 >= cols {
                pendingWrap = true
                cursorCol = cols - 1
            } else {
                cursorCol += 2
            }
        } else {
            writeCell(
                row: cursorRow,
                col: cursorCol,
                cell: TerminalCell(scalar: scalar, fg: currentFg, bg: currentBg, attrs: attrs)
            )
            if cursorCol + 1 >= cols {
                pendingWrap = true
            } else {
                cursorCol += 1
            }
        }
    }

    private func lineFeed() {
        pendingWrap = false
        if cursorRow == scrollBottom {
            scrollUp(top: scrollTop, bottom: scrollBottom, count: 1)
        } else if cursorRow < rows - 1 {
            cursorRow += 1
        }
    }

    private func reverseIndex() {
        pendingWrap = false
        if cursorRow == scrollTop {
            scrollDown(top: scrollTop, bottom: scrollBottom, count: 1)
        } else if cursorRow > 0 {
            cursorRow -= 1
        }
    }

    private func scrollUp(top: Int, bottom: Int, count: Int) {
        guard top <= bottom, top >= 0, bottom < rows, count > 0 else { return }
        let n = min(count, bottom - top + 1)
        let recordScrollback = !inAltScreen && top == 0 && bottom == rows - 1
        for _ in 0..<n {
            if recordScrollback && maxScrollback > 0 {
                scrollback.append(primaryGrid[top])
                if scrollback.count > maxScrollback {
                    scrollback.removeFirst(scrollback.count - maxScrollback)
                }
            }
            for r in top..<bottom {
                currentRowSlice(r, set: activeGridRead(r + 1))
            }
            currentRowSlice(bottom, set: Array(repeating: .blank, count: cols))
        }
    }

    private func scrollDown(top: Int, bottom: Int, count: Int) {
        guard top <= bottom, top >= 0, bottom < rows, count > 0 else { return }
        let n = min(count, bottom - top + 1)
        for _ in 0..<n {
            for r in stride(from: bottom, to: top, by: -1) {
                currentRowSlice(r, set: activeGridRead(r - 1))
            }
            currentRowSlice(top, set: Array(repeating: .blank, count: cols))
        }
    }

    private func saveCursor() {
        let snap = (cursorRow, cursorCol, currentFg, currentBg, currentAttrs)
        if inAltScreen {
            altSavedCursor = snap
        } else {
            primarySavedCursor = snap
        }
    }

    private func restoreCursor() {
        let snap = inAltScreen ? altSavedCursor : primarySavedCursor
        cursorRow = min(max(0, snap.0), rows - 1)
        cursorCol = min(max(0, snap.1), cols - 1)
        currentFg = snap.2
        currentBg = snap.3
        currentAttrs = snap.4
        pendingWrap = false
    }

    // MARK: - CSI Execution

    private func executeCSI(final: UInt8) {
        let raw = String(bytes: csiBytes, encoding: .utf8) ?? ""
        let isPrivate = raw.hasPrefix("?")
        let paramBody = isPrivate ? String(raw.dropFirst()) : raw

        if final == 0x6D /* 'm' */ && !isPrivate {
            applySGR(paramBody)
            return
        }

        let params: [Int] = paramBody.isEmpty ? [] : paramBody.split(separator: ";", omittingEmptySubsequences: false).map {
            Int($0) ?? 0
        }
        let p0 = params.first ?? 0
        let p0Pos = max(1, p0 == 0 ? 1 : p0)

        if isPrivate {
            let enable = final == 0x68 /* 'h' */
            if final == 0x68 || final == 0x6C /* 'l' */ {
                for mode in params {
                    switch mode {
                    case 1:
                        applicationCursorKeys = enable
                    case 25:
                        cursorVisible = enable
                    case 2004:
                        bracketedPasteMode = enable
                    case 47, 1047, 1049:
                        if enable && !inAltScreen {
                            if mode == 1049 { saveCursor() }
                            altGrid = Self.makeGrid(cols: cols, rows: rows)
                            inAltScreen = true
                            cursorRow = 0
                            cursorCol = 0
                            scrollTop = 0
                            scrollBottom = rows - 1
                            pendingWrap = false
                        } else if !enable && inAltScreen {
                            inAltScreen = false
                            scrollTop = 0
                            scrollBottom = rows - 1
                            if mode == 1049 { restoreCursor() }
                        }
                    default:
                        break
                    }
                }
            }
            return
        }

        switch final {
        case 0x41: // CUU
            pendingWrap = false
            cursorRow = max(scrollTop, cursorRow - p0Pos)
        case 0x42: // CUD
            pendingWrap = false
            cursorRow = min(scrollBottom, cursorRow + p0Pos)
        case 0x43: // CUF
            pendingWrap = false
            cursorCol = min(cols - 1, cursorCol + p0Pos)
        case 0x44: // CUB
            pendingWrap = false
            cursorCol = max(0, cursorCol - p0Pos)
        case 0x45: // CNL
            pendingWrap = false
            cursorCol = 0
            cursorRow = min(scrollBottom, cursorRow + p0Pos)
        case 0x46: // CPL
            pendingWrap = false
            cursorCol = 0
            cursorRow = max(scrollTop, cursorRow - p0Pos)
        case 0x47, 0x60: // CHA, HPA
            pendingWrap = false
            cursorCol = min(max(0, p0Pos - 1), cols - 1)
        case 0x48, 0x66: // CUP, HVP
            pendingWrap = false
            let r = (params.count >= 1 && params[0] > 0) ? params[0] : 1
            let c = (params.count >= 2 && params[1] > 0) ? params[1] : 1
            cursorRow = min(max(0, r - 1), rows - 1)
            cursorCol = min(max(0, c - 1), cols - 1)
        case 0x64: // VPA
            pendingWrap = false
            cursorRow = min(max(0, p0Pos - 1), rows - 1)
        case 0x4A: // ED
            pendingWrap = false
            eraseDisplay(mode: p0)
        case 0x4B: // EL
            pendingWrap = false
            eraseLine(mode: p0)
        case 0x4C: // IL
            pendingWrap = false
            if cursorRow >= scrollTop && cursorRow <= scrollBottom {
                scrollDown(top: cursorRow, bottom: scrollBottom, count: p0Pos)
            }
        case 0x4D: // DL
            pendingWrap = false
            if cursorRow >= scrollTop && cursorRow <= scrollBottom {
                scrollUp(top: cursorRow, bottom: scrollBottom, count: p0Pos)
            }
        case 0x40: // ICH
            pendingWrap = false
            let n = min(p0Pos, cols - cursorCol)
            var row = activeGridRead(cursorRow)
            for c in stride(from: cols - 1, through: cursorCol + n, by: -1) {
                row[c] = row[c - n]
            }
            for c in cursorCol..<(cursorCol + n) {
                row[c] = .blank
            }
            currentRowSlice(cursorRow, set: row)
        case 0x50: // DCH
            pendingWrap = false
            let n = min(p0Pos, cols - cursorCol)
            var row = activeGridRead(cursorRow)
            for c in cursorCol..<(cols - n) {
                row[c] = row[c + n]
            }
            for c in (cols - n)..<cols {
                row[c] = .blank
            }
            currentRowSlice(cursorRow, set: row)
        case 0x58: // ECH
            pendingWrap = false
            let n = min(p0Pos, cols - cursorCol)
            for c in cursorCol..<(cursorCol + n) {
                writeCell(row: cursorRow, col: c, cell: .blank)
            }
        case 0x53: // SU
            scrollUp(top: scrollTop, bottom: scrollBottom, count: p0Pos)
        case 0x54: // SD
            scrollDown(top: scrollTop, bottom: scrollBottom, count: p0Pos)
        case 0x72: // DECSTBM
            let t = ((params.count >= 1 && params[0] > 0) ? params[0] : 1) - 1
            let b = ((params.count >= 2 && params[1] > 0) ? params[1] : rows) - 1
            if t < b && t >= 0 && b < rows {
                scrollTop = t
                scrollBottom = b
                cursorRow = 0
                cursorCol = 0
                pendingWrap = false
            }
        case 0x73: // SCP
            saveCursor()
        case 0x75: // RCP
            restoreCursor()
        default:
            break
        }
    }

    private func eraseDisplay(mode: Int) {
        switch mode {
        case 0:
            eraseLine(mode: 0)
            for r in (cursorRow + 1)..<rows {
                currentRowSlice(r, set: Array(repeating: .blank, count: cols))
            }
        case 1:
            for r in 0..<cursorRow {
                currentRowSlice(r, set: Array(repeating: .blank, count: cols))
            }
            eraseLine(mode: 1)
        case 2, 3:
            if mode == 3 && !inAltScreen {
                scrollback.removeAll(keepingCapacity: true)
            }
            for r in 0..<rows {
                currentRowSlice(r, set: Array(repeating: .blank, count: cols))
            }
        default:
            break
        }
    }

    private func eraseLine(mode: Int) {
        let blankCell = TerminalCell(scalar: " ", fg: .defaultFg, bg: currentBg, attrs: [])
        switch mode {
        case 0:
            for c in cursorCol..<cols {
                writeCell(row: cursorRow, col: c, cell: blankCell)
            }
        case 1:
            for c in 0...min(cursorCol, cols - 1) {
                writeCell(row: cursorRow, col: c, cell: blankCell)
            }
        case 2:
            currentRowSlice(cursorRow, set: Array(repeating: blankCell, count: cols))
        default:
            break
        }
    }

    private func applySGR(_ raw: String) {
        if raw.isEmpty {
            currentFg = .defaultFg
            currentBg = .defaultBg
            currentAttrs = []
            return
        }
        // Normalize colon sub-parameters (e.g. `38:2::r:g:b` or `38:2:r:g:b`) into integer tokens.
        let tokens: [Int] = raw.split(separator: ";", omittingEmptySubsequences: false).flatMap { part -> [Int] in
            if part.contains(":") {
                let sub = part.split(separator: ":", omittingEmptySubsequences: false).map { Int($0) ?? 0 }
                // Collapse `38:2:cs:r:g:b` -> `38, 2, r, g, b`
                if (sub.first == 38 || sub.first == 48) && sub.count >= 6 && sub[1] == 2 {
                    return [sub[0], 2, sub[3], sub[4], sub[5]]
                }
                return sub
            }
            return [Int(part) ?? 0]
        }

        var i = 0
        while i < tokens.count {
            let code = tokens[i]
            switch code {
            case 0:
                currentFg = .defaultFg
                currentBg = .defaultBg
                currentAttrs = []
            case 1: currentAttrs.insert(.bold)
            case 2: currentAttrs.insert(.dim)
            case 3: currentAttrs.insert(.italic)
            case 4: currentAttrs.insert(.underline)
            case 7: currentAttrs.insert(.inverse)
            case 9: currentAttrs.insert(.strikethrough)
            case 22:
                currentAttrs.remove(.bold)
                currentAttrs.remove(.dim)
            case 23: currentAttrs.remove(.italic)
            case 24: currentAttrs.remove(.underline)
            case 27: currentAttrs.remove(.inverse)
            case 29: currentAttrs.remove(.strikethrough)
            case 30...37:
                currentFg = .indexed(UInt8(code - 30))
            case 39:
                currentFg = .defaultFg
            case 40...47:
                currentBg = .indexed(UInt8(code - 40))
            case 49:
                currentBg = .defaultBg
            case 90...97:
                currentFg = .indexed(UInt8(code - 90 + 8))
            case 100...107:
                currentBg = .indexed(UInt8(code - 100 + 8))
            case 38, 48:
                let isFg = (code == 38)
                if i + 2 < tokens.count && tokens[i + 1] == 5 {
                    let idx = UInt8(clamping: tokens[i + 2])
                    if isFg { currentFg = .indexed(idx) } else { currentBg = .indexed(idx) }
                    i += 2
                } else if i + 4 < tokens.count && tokens[i + 1] == 2 {
                    let r = UInt8(clamping: tokens[i + 2])
                    let g = UInt8(clamping: tokens[i + 3])
                    let b = UInt8(clamping: tokens[i + 4])
                    if isFg { currentFg = .rgb(r, g, b) } else { currentBg = .rgb(r, g, b) }
                    i += 4
                }
            default:
                break
            }
            i += 1
        }
    }

    // MARK: - OSC Parsing

    private func executeOSC() {
        guard let str = String(bytes: oscBytes, encoding: .utf8),
              let semi = str.firstIndex(of: ";")
        else { return }
        let code = String(str[..<semi])
        let payload = String(str[str.index(after: semi)...])
        switch code {
        case "0", "2":
            title = payload
        case "7":
            if payload.hasPrefix("file://"),
               let url = URL(string: payload), !url.path.isEmpty {
                cwd = url.path
            } else if !payload.isEmpty {
                cwd = payload
            }
        case "7501":
            parseOSC7501(payload)
        default:
            break
        }
    }

    private func parseOSC7501(_ payload: String) {
        var state: ProgramState?
        var app: String?
        var kind: String?
        var msg: String?
        var label: String?
        for item in payload.split(separator: ";", omittingEmptySubsequences: true) {
            guard let eq = item.firstIndex(of: "=") else { continue }
            let k = String(item[..<eq]).lowercased()
            let v = String(item[item.index(after: eq)...])
            switch k {
            case "state": state = ProgramState(rawValue: v.lowercased())
            case "app": app = v.isEmpty ? nil : v
            case "kind": kind = v.isEmpty ? nil : v
            case "msg": msg = v.isEmpty ? nil : v
            case "label": label = v.isEmpty ? nil : v
            default: break
            }
        }
        if let state {
            agentStatus = ProgramStatus(state: state, app: app, kind: kind, msg: msg, label: label)
        }
    }

    // MARK: - Helpers

    private static func makeGrid(cols: Int, rows: Int) -> [[TerminalCell]] {
        let blankRow = Array(repeating: TerminalCell.blank, count: cols)
        return Array(repeating: blankRow, count: rows)
    }

    private static func resizedGrid(
        _ old: [[TerminalCell]],
        oldCols: Int,
        newCols: Int,
        newRows: Int
    ) -> [[TerminalCell]] {
        var next = makeGrid(cols: newCols, rows: newRows)
        let copyRows = min(old.count, newRows)
        let copyCols = min(oldCols, newCols)
        for r in 0..<copyRows {
            for c in 0..<copyCols {
                next[r][c] = old[r][c]
            }
        }
        return next
    }

    private static func displayWidth(_ scalar: UnicodeScalar) -> Int {
        let v = scalar.value
        if v < 0x20 || v == 0x7F { return 0 }
        // Combining diacritics / zero-width joiners.
        if (0x0300...0x036F).contains(v)
            || (0x1AB0...0x1AFF).contains(v)
            || (0x1DC0...0x1DFF).contains(v)
            || (0x20D0...0x20FF).contains(v)
            || (0xFE00...0xFE0F).contains(v)
            || v == 0x200D
        {
            return 0
        }
        // Common East Asian wide / emoji blocks.
        if (0x1100...0x115F).contains(v)
            || (0x2E80...0x9FFF).contains(v)
            || (0xAC00...0xD7AF).contains(v)
            || (0xF900...0xFAFF).contains(v)
            || (0xFE10...0xFE6F).contains(v)
            || (0xFF01...0xFF60).contains(v)
            || (0xFFE0...0xFFE6).contains(v)
            || (0x1F300...0x1FAFF).contains(v)
        {
            return 2
        }
        return 1
    }
}
