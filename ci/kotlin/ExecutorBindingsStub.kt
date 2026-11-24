package ptpsim.ci

import io.fujikage.ptpsim.cameraprotocol.BleExecutorTransport
import io.fujikage.ptpsim.cameraprotocol.CccdMode
import io.fujikage.ptpsim.cameraprotocol.ConnectionActivityEvent
import io.fujikage.ptpsim.cameraprotocol.ConnectionActivityFailure
import io.fujikage.ptpsim.cameraprotocol.ConnectionActivityObserver
import io.fujikage.ptpsim.cameraprotocol.ConnectionActivityRetry
import io.fujikage.ptpsim.cameraprotocol.KeyValue
import io.fujikage.ptpsim.cameraprotocol.ReadyReconnectRecognition
import io.fujikage.ptpsim.cameraprotocol.ScanObservation
import io.fujikage.ptpsim.cameraprotocol.ConfigStore
import io.fujikage.ptpsim.cameraprotocol.ExecutorException
import io.fujikage.ptpsim.cameraprotocol.ExecutorStepFailureKind
import io.fujikage.ptpsim.cameraprotocol.Predicate
import io.fujikage.ptpsim.cameraprotocol.PredicateOp
import io.fujikage.ptpsim.cameraprotocol.PtpExecutorTransport
import io.fujikage.ptpsim.cameraprotocol.PtpSessionOpenResult
import io.fujikage.ptpsim.cameraprotocol.SocketRole
import io.fujikage.ptpsim.cameraprotocol.Step
import io.fujikage.ptpsim.cameraprotocol.StepObserver
import io.fujikage.ptpsim.cameraprotocol.StepReport

fun fallbackModelBindingShape(store: ConfigStore, model: String): String {
    val fallback: Boolean? = store.isFallbackModel(model)
    return when (fallback) {
        true -> "baseline"
        false -> "specific"
        null -> "unknown"
    }
}

fun readyReconnectBindingShape(
    store: ConfigStore, observation: ScanObservation, persistedScope: List<KeyValue>,
): List<Pair<String, Boolean>> = when (
    val result = store.recognizeReadyForReconnect(observation, persistedScope)
) {
    is ReadyReconnectRecognition.NoMatch -> emptyList()
    is ReadyReconnectRecognition.Candidate ->
        listOf(result.candidate.model to result.candidate.isFallback)
    is ReadyReconnectRecognition.Disambiguate ->
        result.candidates.map { it.model to it.isFallback }
}

class ExecutorBindingsStub : BleExecutorTransport {
    override suspend fun connect() = Unit

    override suspend fun awaitDisconnect() = Unit

    override suspend fun requestMtu(mtu: UShort): UShort = mtu

    override suspend fun ensureServicesDiscovered() = Unit

    override suspend fun read(characteristic: String): ByteArray = byteArrayOf()

    override suspend fun peripheralName(): String = ""

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
