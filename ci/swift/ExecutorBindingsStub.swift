import Foundation

final class ConnectionActivityObserverStub: ConnectionActivityObserver, @unchecked Sendable {
    func onActivity(event: ConnectionActivityEvent) {}
}

final class PtpExecutorTransportStub: PtpExecutorTransport, @unchecked Sendable {
    func reserveTransactionId() async throws -> UInt32 { 1 }
    func sendCommandFrame(frame: Data) async throws {}
    func nextCommandFrame() async throws -> Data { Data() }
    func nextEventFrame(eventCode: UInt16) async throws -> Data { Data() }
    func openChannel(role: SocketRole) async throws {}
    func closeCommandChannel(transportCloseFrame: Data?) async throws {}
    func reopenCommandSession() async throws -> PtpSessionOpenResult {
        PtpSessionOpenResult(transactionId: 1, responseCode: 0x2001, responseParams: [])
    }
    func sleep(ms: UInt32) async throws {}
}

func classifyFailure(_ kind: ExecutorStepFailureKind) -> Bool {
    switch kind {
    case .deadlineExceeded, .other:
        return false
    case .conditionRejected:
        return true
    }
}

func stepFailureContext(_ error: ExecutorError) -> [KeyValue] {
    guard case .StepFailed(step: _, kind: _, detail: _, context: let context) = error else {
        return []
    }
    return context
}

func activityRetryCount(_ event: ConnectionActivityEvent) -> UInt32 {
    switch event {
    case .started:
        return 0
    case let .retrying(_, _, retry):
        return retry.ordinal
    case let .succeeded(_, _, summary), let .cancelled(_, _, summary):
        return summary.retryCount
    case let .failed(_, _, summary, failure):
        return summary.retryCount + UInt32(failure.context.count)
    }
}

func activityRetryBindingShape() -> ConnectionActivityRetry {
    ConnectionActivityRetry(
        ordinal: 2,
        limit: 3,
        failure: ConnectionActivityFailure(kind: .conditionRejected, context: [])
    )
}

func retryBindingShape() -> Step {
    .retry(
        steps: [],
        whenFailure: .conditionRejected,
        onFailure: [],
        retryWhen: Predicate(field: "detail", op: .eq, value: "2"),
        maxAttempts: 2,
        retryDelayMs: 200,
        failureContext: ["detail"]
    )
}
