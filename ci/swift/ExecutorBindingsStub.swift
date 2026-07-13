import Foundation

final class ConnectionActivityObserverStub: ConnectionActivityObserver, @unchecked Sendable {
    func onActivity(event: ConnectionActivityEvent) {}
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
