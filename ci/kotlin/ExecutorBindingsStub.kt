package ptpsim.ci

import dev.ptpsim.cameraprotocol.BleExecutorTransport
import dev.ptpsim.cameraprotocol.CccdMode
import dev.ptpsim.cameraprotocol.ConnectionActivityEvent
import dev.ptpsim.cameraprotocol.ConnectionActivityFailure
import dev.ptpsim.cameraprotocol.ConnectionActivityObserver
import dev.ptpsim.cameraprotocol.ConnectionActivityRetry
import dev.ptpsim.cameraprotocol.KeyValue
import dev.ptpsim.cameraprotocol.ReadyReconnectRecognition
import dev.ptpsim.cameraprotocol.ScanObservation
import dev.ptpsim.cameraprotocol.ConfigStore
import dev.ptpsim.cameraprotocol.ExecutorException
import dev.ptpsim.cameraprotocol.ExecutorStepFailureKind
import dev.ptpsim.cameraprotocol.Predicate
import dev.ptpsim.cameraprotocol.PredicateOp
import dev.ptpsim.cameraprotocol.PtpIpFrameLengthException
import dev.ptpsim.cameraprotocol.PtpIpFramePrefix
import dev.ptpsim.cameraprotocol.PtpFraming
import dev.ptpsim.cameraprotocol.buildCommand
import dev.ptpsim.cameraprotocol.inspectPtpIpFramePrefix
import dev.ptpsim.cameraprotocol.PtpExecutorTransport
import dev.ptpsim.cameraprotocol.PtpSessionOpenResult
import dev.ptpsim.cameraprotocol.SocketRole
import dev.ptpsim.cameraprotocol.Step
import dev.ptpsim.cameraprotocol.StepObserver
import dev.ptpsim.cameraprotocol.StepReport

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

fun main() {
    val prefix = inspectPtpIpFramePrefix(byteArrayOf()) as PtpIpFramePrefix.NeedMore
    val requiredBytes = prefix.requiredBytes.toInt()
    val command = buildCommand(PtpFraming.STANDARD, 0x1001u.toUShort(), 1u, emptyList())
    val ready = inspectPtpIpFramePrefix(command.copyOf(requiredBytes)) as PtpIpFramePrefix.Ready
    check(ready.totalBytes.toInt() == command.size)

    val invalidPrefix = ByteArray(requiredBytes) { 0xff.toByte() }
    val error = runCatching { inspectPtpIpFramePrefix(invalidPrefix) }.exceptionOrNull()
    check(error is PtpIpFrameLengthException.AboveMaximum && error.declared == UInt.MAX_VALUE)
    println("Kotlin stream frame prefix binding check passed")
}
