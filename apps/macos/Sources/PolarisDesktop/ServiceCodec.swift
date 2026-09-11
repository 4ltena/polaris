import Foundation

/// P5 fake-service wire values. Decimal u64 fields stay strings, never Double.
indirect enum ServiceValue: Sendable, Equatable {
  case object([String: ServiceValue])
  case array([ServiceValue])
  case string(String)
  case number(String)
  case bool(Bool)
  subscript(_ key: String) -> ServiceValue? {
    if case .object(let fields) = self { return fields[key] }
    return nil
  }
  var string: String? {
    if case .string(let value) = self { return value }
    return nil
  }
  // Rust's opaque IDs compare bytes, unlike Swift's canonical-equivalent String equality.
  func matchesID(_ id: String) -> Bool { string?.utf8.elementsEqual(id.utf8) == true }
  var decimal: UInt64? {
    guard let s = string, !s.isEmpty, s == "0" || !s.hasPrefix("0"),
      s.utf8.allSatisfy({ (48...57).contains($0) })
    else { return nil }
    return UInt64(s)
  }
}

enum ServiceError: Error, Sendable, Equatable {
  case frameTooLarge, truncatedFrame, invalidJSON, schema, version, correlation
  case notReady, closed, eof, timeout, cancelled, capacity, io, launch, invalidHelper
}

/// Strict incremental framing, including escaped duplicate keys and nested null.
struct ServiceCodec {
  static let maxFrameBytes = 1_048_576
  private var buffer = Data()
  var hasPartialFrame: Bool { !buffer.isEmpty }

  mutating func receive(_ data: Data) throws -> [ServiceValue] {
    // Callers read at most 16 KiB per tick; no advertised body is allocated.
    guard data.count <= 16_384, buffer.count + data.count <= Self.maxFrameBytes + 4 + 16_384 else {
      throw ServiceError.frameTooLarge
    }
    buffer.append(data)
    var frames: [ServiceValue] = []
    while buffer.count >= 4 {
      let size = buffer.prefix(4).reduce(0) { ($0 << 8) | Int($1) }
      guard size <= Self.maxFrameBytes else { throw ServiceError.frameTooLarge }
      guard buffer.count >= size + 4 else { break }
      frames.append(try Self.json(Data(buffer.dropFirst(4).prefix(size))))
      buffer.removeFirst(size + 4)
    }
    return frames
  }

  func finish() throws { if hasPartialFrame { throw ServiceError.truncatedFrame } }

  static func json(_ data: Data) throws -> ServiceValue {
    guard data.count <= maxFrameBytes else { throw ServiceError.frameTooLarge }
    guard String(data: data, encoding: .utf8) != nil else { throw ServiceError.invalidJSON }
    var parser = Parser(bytes: Array(data))
    let value = try parser.value(depth: 0)
    parser.space()
    guard parser.index == parser.bytes.count else { throw ServiceError.invalidJSON }
    return value
  }

  static func encode(_ value: ServiceValue) throws -> Data {
    var body = Data()
    func append(_ bytes: Data) throws {
      guard bytes.count <= maxFrameBytes - body.count else { throw ServiceError.frameTooLarge }
      body.append(bytes)
    }
    func write(_ value: ServiceValue, depth: Int) throws {
      guard depth < 128 else { throw ServiceError.invalidJSON }
      switch value {
      case .string(let s):
        guard s.utf8.count <= maxFrameBytes else { throw ServiceError.frameTooLarge }
        try append(JSONEncoder().encode(s))
      case .number(let n):
        guard case .number = try json(Data(n.utf8)) else { throw ServiceError.invalidJSON }
        try append(Data(n.utf8))
      case .bool(let b): try append(Data((b ? "true" : "false").utf8))
      case .array(let items):
        try append(Data("[".utf8))
        for (i, item) in items.enumerated() {
          if i > 0 { try append(Data(",".utf8)) }
          try write(item, depth: depth + 1)
        }
        try append(Data("]".utf8))
      case .object(let fields):
        try append(Data("{".utf8))
        for (i, key) in fields.keys.sorted().enumerated() {
          if i > 0 { try append(Data(",".utf8)) }
          try write(.string(key), depth: depth + 1)
          try append(Data(":".utf8))
          try write(fields[key]!, depth: depth + 1)
        }
        try append(Data("}".utf8))
      }
    }
    try write(value, depth: 0)
    let count = UInt32(body.count)
    var frame = Data([
      UInt8((count >> 24) & 255), UInt8((count >> 16) & 255), UInt8((count >> 8) & 255),
      UInt8(count & 255),
    ])
    frame.append(body)
    return frame
  }

  private struct Parser {
    let bytes: [UInt8]
    var index = 0
    mutating func space() {
      while index < bytes.count && [9, 10, 13, 32].contains(bytes[index]) { index += 1 }
    }
    mutating func take(_ byte: UInt8) -> Bool {
      space()
      guard index < bytes.count, bytes[index] == byte else { return false }
      index += 1
      return true
    }
    mutating func text() throws -> String {
      space()
      let start = index
      guard take(34) else { throw ServiceError.invalidJSON }
      while index < bytes.count {
        let b = bytes[index]
        index += 1
        if b == 92 {
          guard index < bytes.count else { break }
          index += 1
        } else if b == 34 {
          do {
            return try JSONDecoder().decode(String.self, from: Data(bytes[start..<index]))
          } catch { throw ServiceError.invalidJSON }
        }
      }
      throw ServiceError.invalidJSON
    }
    mutating func value(depth: Int) throws -> ServiceValue {
      guard depth < 128 else { throw ServiceError.invalidJSON }
      space()
      guard index < bytes.count else { throw ServiceError.invalidJSON }
      if bytes[index] == 34 { return .string(try text()) }
      if take(123) {
        var fields: [String: ServiceValue] = [:]
        if take(125) { return .object(fields) }
        repeat {
          let key = try text()
          guard fields[key] == nil, take(58) else { throw ServiceError.invalidJSON }
          fields[key] = try value(depth: depth + 1)
          if take(125) { return .object(fields) }
        } while take(44)
        throw ServiceError.invalidJSON
      }
      if take(91) {
        var values: [ServiceValue] = []
        if take(93) { return .array(values) }
        repeat {
          values.append(try value(depth: depth + 1))
          if take(93) { return .array(values) }
        } while take(44)
        throw ServiceError.invalidJSON
      }
      let start = index
      while index < bytes.count && ![9, 10, 13, 32, 44, 93, 125].contains(bytes[index]) {
        index += 1
      }
      let token = String(decoding: bytes[start..<index], as: UTF8.self)
      if token == "true" { return .bool(true) }
      if token == "false" { return .bool(false) }
      guard !token.isEmpty, token != "null",
        let number = try? JSONDecoder().decode(Double.self, from: Data(token.utf8)), number.isFinite
      else {
        throw ServiceError.invalidJSON
      }
      return .number(token)
    }
  }
}
