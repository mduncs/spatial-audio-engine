import SwiftUI

struct ContentView: View {
    @EnvironmentObject private var host: FightboxHostModel

    var body: some View {
        TabView {
            NavigationView {
                MonitorView()
            }
            .navigationViewStyle(.stack)
            .tabItem {
                Label("Monitor", systemImage: "waveform")
            }

            NavigationView {
                AbxScaffoldView()
            }
            .navigationViewStyle(.stack)
            .tabItem {
                Label("ABX", systemImage: "ear.and.waveform")
            }
        }
    }
}

private struct MonitorView: View {
    @EnvironmentObject private var host: FightboxHostModel

    var body: some View {
        Form {
            Section("Session") {
                statusRow("Engine", value: host.lifecycleStatus)
                statusRow("Output route", value: host.outputRouteStatus)
                statusRow("Cell streaming", value: host.cellStreamingStatus)
                statusRow("Canonical assets", value: host.canonicalAssetStatus)
                statusRow("Head tracking", value: host.motionStatus)
                statusRow("GPS / local ENU", value: host.gpsStatus)

                Button(host.isRunning ? "Stop Session" : "Start Session") {
                    host.isRunning ? host.stop() : host.start()
                }
                .accessibilityHint(
                    host.isRunning
                        ? "Stops audio, GPS, motion tracking, and route qualification"
                        : "Starts audio only after this explicit action and admits the exact installed production route when available"
                )

                Button(
                    host.routeQualificationInProgress
                        ? "Running Exact 4-Cell Route…"
                        : "Run Exact 4-Cell Route"
                ) {
                    host.runRouteQualification()
                }
                .disabled(
                    !host.routeQualificationAvailable ||
                        host.routeQualificationInProgress
                )
                .accessibilityHint(
                    "Runs the three verified cell transitions while the current audio session remains active; GPS position is paused during the deterministic qualification"
                )
            }


            Section {
                Picker(
                    "Source",
                    selection: Binding(
                        get: { host.selectedSourceContractID },
                        set: host.selectNamedSourceContract
                    )
                ) {
                    ForEach(host.namedSourceContracts) { contract in
                        Text(contract.displayName).tag(contract.id)
                    }
                }
                .pickerStyle(.menu)
            } header: {
                Text("Named source contract")
            } footer: {
                Text("Metadata-only selection. The app contains no Tom's Diner media; supply a licensed canonical package at deployment.")
            }

            Section {
                HStack {
                    Text("Monitor gain")
                    Spacer()
                    Text(host.monitorGainDB, format: .number.precision(.fractionLength(0)))
                        .monospacedDigit()
                    Text("dB")
                        .foregroundStyle(.secondary)
                }
                Slider(
                    value: Binding(
                        get: { host.monitorGainDB },
                        set: host.setMonitorGainDB
                    ),
                    in: -60 ... 0,
                    step: 1
                )
                .accessibilityLabel("Monitor gain")
                .accessibilityValue("\(Int(host.monitorGainDB)) decibels")
            } footer: {
                Text("Gain is applied by AVAudioEngine after Fightbox rendering.")
            }

            Section("Delivered quality and telemetry") {
                ScrollView(.horizontal) {
                    Text(host.telemetryText)
                        .font(.caption.monospaced())
                        .textSelection(.enabled)
                        .frame(maxWidth: .infinity, alignment: .leading)
                }
                .accessibilityLabel("Fightbox delivered quality telemetry")
            }
        }
        .navigationTitle("Fightbox")
    }

    private func statusRow(_ label: String, value: String) -> some View {
        VStack(alignment: .leading, spacing: 4) {
            Text(label)
                .font(.subheadline.weight(.semibold))
            Text(value)
                .font(.footnote)
                .foregroundStyle(.secondary)
        }
        .accessibilityElement(children: .combine)
    }
}
