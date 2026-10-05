import MapKit

/// The quality dots as one ground overlay, bucketed so each map tile only
/// touches the dots near it.
public final class DotsOverlay: NSObject, MKOverlay, @unchecked Sendable {
    public let coordinate: CLLocationCoordinate2D
    public let boundingMapRect: MKMapRect
    public let take: Take
    let spacing: Double
    private let points: [MKMapPoint]
    private let quality: [Double]
    private let cells: [[Int32]]
    private let columns: Int, rows: Int
    private let cell: Double

    public init(dots: Hello.Dots, frame: GeoFrame, take: Take) {
        self.take = take
        var points: [MKMapPoint] = [], quality: [Double] = []
        for dot in dots.points where dot.count >= 3 {
            points.append(MKMapPoint(frame.coordinate(east: dot[0], north: dot[1])))
            quality.append(dot[2])
        }
        self.points = points
        self.quality = quality
        let center = frame.coordinate(east: 0, north: 0)
        spacing = dots.spacingM * MKMapPointsPerMeterAtLatitude(center.latitude)
        let xs = points.map(\.x), ys = points.map(\.y)
        let pad = spacing * 2
        let rect = points.isEmpty
            ? MKMapRect(origin: MKMapPoint(center), size: MKMapSize(width: 1, height: 1))
            : MKMapRect(x: xs.min()! - pad, y: ys.min()! - pad, width: xs.max()! - xs.min()! + pad * 2,
                        height: ys.max()! - ys.min()! + pad * 2)
        boundingMapRect = rect
        coordinate = rect.origin.coordinate
        cell = max(rect.width, rect.height) / 48
        columns = max(1, Int(rect.width / cell) + 1)
        rows = max(1, Int(rect.height / cell) + 1)
        var cells = Array(repeating: [Int32](), count: columns * rows)
        for (index, point) in points.enumerated() {
            let column = min(columns - 1, Int((point.x - rect.minX) / cell))
            let row = min(rows - 1, Int((point.y - rect.minY) / cell))
            cells[row * columns + column].append(Int32(index))
        }
        self.cells = cells
    }

    func dots(in rect: MKMapRect) -> [(MKMapPoint, Double)] {
        let clipped = rect.intersection(boundingMapRect)
        guard !clipped.isNull else { return [] }
        let first = (Int((clipped.minX - boundingMapRect.minX) / cell), Int((clipped.minY - boundingMapRect.minY) / cell))
        let last = (min(columns - 1, Int((clipped.maxX - boundingMapRect.minX) / cell)),
                    min(rows - 1, Int((clipped.maxY - boundingMapRect.minY) / cell)))
        var found: [(MKMapPoint, Double)] = []
        for row in max(0, first.1)...max(0, last.1) {
            for column in max(0, first.0)...max(0, last.0) {
                for index in cells[row * columns + column] where rect.contains(points[Int(index)]) {
                    found.append((points[Int(index)], quality[Int(index)]))
                }
            }
        }
        return found
    }
}

public final class DotsRenderer: MKOverlayRenderer {
    override public func draw(_ mapRect: MKMapRect, zoomScale: MKZoomScale, in context: CGContext) {
        guard let overlay = overlay as? DotsOverlay else { return }
        let pad = overlay.spacing * 2
        let found = overlay.dots(in: mapRect.insetBy(dx: -pad, dy: -pad))
        DotPainter.draw(context, dots: found.map { (point(for: $0.0), $0.1) }, spacing: CGFloat(overlay.spacing),
                        pixel: 1 / zoomScale, take: overlay.take)
    }
}
