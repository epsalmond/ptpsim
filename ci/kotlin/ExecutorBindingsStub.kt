package ptpsim.ci

import uniffi.camera_protocol_ffi.BleExecutorTransport
import uniffi.camera_protocol_ffi.CccdMode
import uniffi.camera_protocol_ffi.ConnectionActivityEvent
import uniffi.camera_protocol_ffi.ConnectionActivityFailure
import uniffi.camera_protocol_ffi.ConnectionActivityObserver
import uniffi.camera_protocol_ffi.ConnectionActivityRetry
import uniffi.camera_protocol_ffi.ExecutorException
import uniffi.camera_protocol_ffi.ExecutorStepFailureKind
import uniffi.camera_protocol_ffi.Predicate
import uniffi.camera_protocol_ffi.PredicateOp
import uniffi.camera_protocol_ffi.PtpExecutorTransport
import uniffi.camera_protocol_ffi.PtpSessionOpenResult
import uniffi.camera_protocol_ffi.SocketRole
import uniffi.camera_protocol_ffi.Step
import uniffi.camera_protocol_ffi.StepObserver
import uniffi.camera_protocol_ffi.StepReport

class ExecutorBindingsStub : BleExecutorTransport {
    override suspend fun connect() = Unit

    override suspend fun awaitDisconnect() = Unit

    override suspend fun requestMtu(mtu: UShort): UShort = mtu

    override suspend fun ensureServicesDiscovered() = Unit

    override suspend fun read(characteristic: String): ByteArray = byteArrayOf()

    override suspend fun write(characteristic: String, value: ByteArray) = Unit

    override suspend fun writeWithNotificationFence(
        characteristic: String,
        value: ByteArray,
        notificationCharacteristic: String,
    ) = Unit

    override suspend fun subscribe(characteristic: String, mode: CccdMode) = Unit

    override suspend fun nextNotification(characteristic: String): ByteArray = byteArrayOf()

    override suspend fun sleep(ms: UInt) = Unit
}

class PtpExecutorTransportStub : PtpExecutorTransport {
    override suspend fun reserveTransactionId(): UInt = 1u

    override suspend fun sendCommandFrame(frame: ByteArray) = Unit

    override suspend fun nextCommandFrame(): ByteArray = byteArrayOf()

    override suspend fun nextEventFrame(eventCode: UShort): ByteArray = byteArrayOf()

    override suspend fun openChannel(role: SocketRole) = Unit

    override suspend fun closeCommandChannel(transportCloseFrame: ByteArray?) = Unit

    override suspend fun reopenCommandSession(): PtpSessionOpenResult =
        PtpSessionOpenResult(1u, 0x2001u.toUShort(), emptyList())

    override suspend fun sleep(ms: UInt) = Unit
}

class StepObserverStub : StepObserver {
    override fun onStep(report: StepReport) = Unit
}

class ConnectionActivityObserverStub : ConnectionActivityObserver {
    override fun onActivity(event: ConnectionActivityEvent) = Unit
}

fun stepFailureDetail(error: ExecutorException.StepFailed): String =
    "${error.kind}:${error.detail}:${error.context.size}"

fun classifyFailure(kind: ExecutorStepFailureKind): Boolean = when (kind) {
    ExecutorStepFailureKind.DEADLINE_EXCEEDED -> false
    ExecutorStepFailureKind.CONDITION_REJECTED -> true
    ExecutorStepFailureKind.OTHER -> false
}

fun activityRetryCount(event: ConnectionActivityEvent): UInt = when (event) {
    is ConnectionActivityEvent.Started -> 0u
    is ConnectionActivityEvent.Retrying -> event.retry.ordinal
    is ConnectionActivityEvent.Succeeded -> event.summary.retryCount
    is ConnectionActivityEvent.Failed ->
        event.summary.retryCount + event.failure.context.size.toUInt()
    is ConnectionActivityEvent.Cancelled -> event.summary.retryCount
}

fun activityRetryBindingShape(): ConnectionActivityRetry = ConnectionActivityRetry(
    ordinal = 2u,
    limit = 3u,
    failure = ConnectionActivityFailure(
        kind = ExecutorStepFailureKind.CONDITION_REJECTED,
        context = emptyList(),
    ),
)

fun retryBindingShape(): Step = Step.Retry(
    steps = emptyList(),
    whenFailure = ExecutorStepFailureKind.CONDITION_REJECTED,
    onFailure = emptyList(),
    retryWhen = Predicate("detail", PredicateOp.EQ, "2"),
    maxAttempts = 2u,
    retryDelayMs = 200u,
    failureContext = listOf("detail"),
)
