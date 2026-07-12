package ptpsim.ci

import uniffi.camera_protocol_ffi.BleExecutorTransport
import uniffi.camera_protocol_ffi.CccdMode
import uniffi.camera_protocol_ffi.ExecutorException
import uniffi.camera_protocol_ffi.StepObserver
import uniffi.camera_protocol_ffi.StepReport

class ExecutorBindingsStub : BleExecutorTransport {
    override suspend fun connect() = Unit

    override suspend fun awaitDisconnect() = Unit

    override suspend fun requestMtu(mtu: UShort): UShort = mtu

    override suspend fun ensureServicesDiscovered() = Unit

    override suspend fun read(characteristic: String): ByteArray = byteArrayOf()

    override suspend fun write(characteristic: String, value: ByteArray) = Unit

    override suspend fun subscribe(characteristic: String, mode: CccdMode) = Unit

    override suspend fun nextNotification(characteristic: String): ByteArray = byteArrayOf()

    override suspend fun sleep(ms: UInt) = Unit
}

class StepObserverStub : StepObserver {
    override fun onStep(report: StepReport) = Unit
}

fun stepFailureDetail(error: ExecutorException.StepFailed): String = error.detail
