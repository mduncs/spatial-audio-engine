import SwiftUI

struct AbxScaffoldView: View {
    private let session: AbxSession?

    init() {
        session = try? AbxSession(
            sessionID: "host-scaffold",
            listener: AbxListenerIdentity(listenerID: "pending"),
            hrtf: AbxHrtfRecord(
                hrtfSet: "pending",
                pretestResult: "pending"
            ),
            equipment: AbxEquipmentRecord(
                headphones: "pending",
                outputPath: "Fightbox iOS host"
            ),
            device: "iPhone",
            headTrackingEnabled: true,
            seed: 0xF17B_0A,
            dateISO: "pending",
            signOff: AbxSignOff(
                listenerSigned: "pending",
                dateISO: "pending"
            )
        )
    }

    var body: some View {
        Form {
            Section("Externalization ABX") {
                Text(
                    "The deterministic AbxSession plan is wired. A and B stimulus playback remains host-owned and is intentionally not authored here."
                )
                .font(.callout)

                if let trial = session?.nextUnansweredTrial {
                    VStack(alignment: .leading, spacing: 8) {
                        Text("Trial \(trial.trialIndex) of \(session?.trials.count ?? 0)")
                            .font(.headline)
                        Text(
                            "Presentation order: "
                                + trial.presentedOrder.map(\.rawValue).joined(separator: " · ")
                        )
                        .font(.body.monospaced())
                    }
                    .accessibilityElement(children: .combine)
                }
            }

            Section("Stimulus host stub") {
                HStack {
                    stubButton("Present A")
                    stubButton("Present B")
                    stubButton("Present X")
                }
                Text("Connect verified A/B stimuli before enabling playback or response capture.")
                    .font(.footnote)
                    .foregroundStyle(.secondary)
            }

            Section {
                HStack {
                    stubButton("Choose A")
                    stubButton("Choose B")
                }
            } header: {
                Text("Forced choice")
            } footer: {
                Text(FIGHTBOX_ABX_REQUIRES_HUMAN)
            }
        }
        .navigationTitle("ABX")
    }

    private func stubButton(_ title: String) -> some View {
        Button(title) {}
            .buttonStyle(.bordered)
            .disabled(true)
    }
}

