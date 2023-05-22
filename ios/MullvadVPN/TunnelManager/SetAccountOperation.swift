//
//  SetAccountOperation.swift
//  MullvadVPN
//
//  Created by pronebird on 16/12/2021.
//  Copyright © 2021 Mullvad VPN AB. All rights reserved.
//

import Foundation
import MullvadLogging
import MullvadREST
import MullvadTypes
import Operations
import class WireGuardKitTypes.PrivateKey
import class WireGuardKitTypes.PublicKey

enum SetAccountAction {
    /// Set new account.
    case new

    /// Set existing account.
    case existing(String)

    /// Unset account.
    case unset

    var taskName: String {
        switch self {
        case .new:
            return "Set new account"
        case .existing:
            return "Set existing account"
        case .unset:
            return "Unset account"
        }
    }
}

private struct SetAccountResult {
    let accountData: StoredAccountData
    let privateKey: PrivateKey
    let device: REST.Device
}

private struct SetAccountContext: OperationInputContext {
    var accountData: StoredAccountData?
    var privateKey: PrivateKey?
    var device: REST.Device?

    func reduce() -> SetAccountResult? {
        guard let accountData,
              let privateKey,
              let device
        else {
            return nil
        }

        return SetAccountResult(
            accountData: accountData,
            privateKey: privateKey,
            device: device
        )
    }
}

class SetAccountOperation: ResultOperation<StoredAccountData?> {
    private let interactor: TunnelInteractor
    private let accountsProxy: REST.AccountsProxy
    private let devicesProxy: REST.DevicesProxy
    private let action: SetAccountAction

    private let logger = Logger(label: "SetAccountOperation")
    private let operationQueue = AsyncOperationQueue()

    private var children: [Operation] = []

    init(
        dispatchQueue: DispatchQueue,
        interactor: TunnelInteractor,
        accountsProxy: REST.AccountsProxy,
        devicesProxy: REST.DevicesProxy,
        action: SetAccountAction
    ) {
        self.interactor = interactor
        self.accountsProxy = accountsProxy
        self.devicesProxy = devicesProxy
        self.action = action

        super.init(dispatchQueue: dispatchQueue)
    }

    override func main() {
        let deleteDeviceOperation = getDeleteDeviceOperation()
        let unsetDeviceStateOperation = getUnsetDeviceStateOperation()

        deleteDeviceOperation.flatMap { unsetDeviceStateOperation.addDependency($0) }

        let setupAccountOperations = getAccountDataOperation()
            .flatMap { accountOperation -> [Operation] in
                accountOperation.addCondition(
                    NoFailedDependenciesCondition(ignoreCancellations: false)
                )
                accountOperation.addDependency(unsetDeviceStateOperation)

                let createDeviceOperation = getCreateDeviceOperation()
                createDeviceOperation.addCondition(
                    NoFailedDependenciesCondition(ignoreCancellations: false)
                )
                createDeviceOperation.inject(from: accountOperation)

                let saveSettingsOperation = getSaveSettingsOperation()
                saveSettingsOperation.addCondition(
                    NoFailedDependenciesCondition(ignoreCancellations: false)
                )

                saveSettingsOperation.injectMany(context: SetAccountContext())
                    .inject(from: accountOperation, assignOutputTo: \.accountData)
                    .inject(from: createDeviceOperation, via: { context, output in
                        let (privateKey, device) = output

                        context.privateKey = privateKey
                        context.device = device
                    })
                    .reduce()

                saveSettingsOperation.onFinish { operation, error in
                    self.completeOperation(accountData: operation.output)
                }

                return [accountOperation, createDeviceOperation, saveSettingsOperation]
            } ?? []

        var enqueueOperations: [Operation] = [deleteDeviceOperation, unsetDeviceStateOperation]
            .compactMap { $0 }
        enqueueOperations.append(contentsOf: setupAccountOperations)

        if setupAccountOperations.isEmpty {
            let finishingOperation = BlockOperation()
            finishingOperation.completionBlock = { [weak self] in
                self?.completeOperation(accountData: nil)
            }
            finishingOperation.addDependencies(enqueueOperations)
            enqueueOperations.append(finishingOperation)
        }

        children = enqueueOperations
        operationQueue.addOperations(enqueueOperations, waitUntilFinished: false)
    }

    override func operationDidCancel() {
        operationQueue.cancelAllOperations()
    }

    // MARK: - Private

    private func completeOperation(accountData: StoredAccountData?) {
        guard !isCancelled else {
            finish(result: .failure(OperationError.cancelled))
            return
        }

        let errors = children.compactMap { operation -> Error? in
            return (operation as? AsyncOperation)?.error
        }

        if let error = errors.first {
            finish(result: .failure(error))
        } else {
            finish(result: .success(accountData))
        }
    }

    private func getAccountDataOperation() -> ResultOperation<StoredAccountData>? {
        switch action {
        case .new:
            return getCreateAccountOperation()

        case let .existing(accountNumber):
            return getExistingAccountOperation(accountNumber: accountNumber)

        case .unset:
            return nil
        }
    }

    private func getCreateAccountOperation() -> ResultBlockOperation<StoredAccountData> {
        return ResultBlockOperation<StoredAccountData>(dispatchQueue: dispatchQueue) { finish -> Cancellable in
            self.logger.debug("Create new account...")

            return self.accountsProxy.createAccount(retryStrategy: .default) { result in
                let result = result.inspectError { error in
                    guard !error.isOperationCancellationError else { return }

                    self.logger.error(
                        error: error,
                        message: "Failed to create new account."
                    )
                }.map { newAccountData -> StoredAccountData in
                    self.logger.debug("Created new account.")

                    return StoredAccountData(
                        identifier: newAccountData.id,
                        number: newAccountData.number,
                        expiry: newAccountData.expiry
                    )
                }

                finish(result)
            }
        }
    }

    private func getExistingAccountOperation(accountNumber: String) -> ResultOperation<StoredAccountData> {
        return ResultBlockOperation<StoredAccountData>(dispatchQueue: dispatchQueue) { finish -> Cancellable in
            self.logger.debug("Request account data...")

            return self.accountsProxy
                .getAccountData(accountNumber: accountNumber, retryStrategy: .default) { result in
                    let result = result.inspectError { error in
                        guard !error.isOperationCancellationError else { return }

                        self.logger.error(
                            error: error,
                            message: "Failed to receive account data."
                        )
                    }.map { accountData -> StoredAccountData in
                        self.logger.debug("Received account data.")

                        return StoredAccountData(
                            identifier: accountData.id,
                            number: accountNumber,
                            expiry: accountData.expiry
                        )
                    }

                    finish(result)
                }
        }
    }

    private func getDeleteDeviceOperation() -> AsyncBlockOperation? {
        guard case let .loggedIn(accountData, deviceData) = interactor.deviceState else {
            return nil
        }

        let operation = AsyncBlockOperation(dispatchQueue: dispatchQueue) { finish -> Cancellable in
            self.logger.debug("Delete current device...")

            return self.devicesProxy.deleteDevice(
                accountNumber: accountData.number,
                identifier: deviceData.identifier,
                retryStrategy: .default
            ) { result in
                switch result {
                case let .success(isDeleted):
                    self.logger.debug(isDeleted ? "Deleted device." : "Device is already deleted.")

                case let .failure(error) where !error.isOperationCancellationError:
                    self.logger.error(
                        error: error,
                        message: "Failed to delete device."
                    )

                default:
                    break
                }

                finish(result.error)
            }
        }

        return operation
    }

    private func getUnsetDeviceStateOperation() -> AsyncBlockOperation {
        return AsyncBlockOperation(dispatchQueue: dispatchQueue) { finish in
            // Tell the caller to unsubscribe from VPN status notifications.
            self.interactor.prepareForVPNConfigurationDeletion()

            // Reset tunnel and device state.
            self.interactor.updateTunnelStatus { tunnelStatus in
                tunnelStatus = TunnelStatus()
                tunnelStatus.state = .disconnected
            }
            self.interactor.setDeviceState(.loggedOut, persist: true)

            // Finish immediately if tunnel provider is not set.
            guard let tunnel = self.interactor.tunnel else {
                finish(nil)
                return
            }

            // Remove VPN configuration.
            tunnel.removeFromPreferences { error in
                self.dispatchQueue.async {
                    // Ignore error but log it.
                    if let error {
                        self.logger.error(
                            error: error,
                            message: "Failed to remove VPN configuration."
                        )
                    }

                    self.interactor.setTunnel(nil, shouldRefreshTunnelState: false)

                    finish(nil)
                }
            }
        }
    }

    private func getCreateDeviceOperation() -> TransformOperation<StoredAccountData, (PrivateKey, REST.Device)> {
        return TransformOperation<StoredAccountData, (
            PrivateKey,
            REST.Device
        )>(dispatchQueue: dispatchQueue) { storedAccountData, finish -> Cancellable in
            self.logger.debug("Store last used account.")

            do {
                try SettingsManager.setLastUsedAccount(storedAccountData.number)
            } catch {
                self.logger.error(
                    error: error,
                    message: "Failed to store last used account number."
                )
            }

            self.logger.debug("Create device...")

            let privateKey = PrivateKey()

            let request = REST.CreateDeviceRequest(
                publicKey: privateKey.publicKey,
                hijackDNS: false
            )

            return self.devicesProxy.createDevice(
                accountNumber: storedAccountData.number,
                request: request,
                retryStrategy: .default
            ) { result in
                let result = result
                    .map { device in
                        return (privateKey, device)
                    }
                    .inspectError { error in
                        self.logger.error(error: error, message: "Failed to create device.")
                    }

                finish(result)
            }
        }
    }

    private func getSaveSettingsOperation() -> TransformOperation<SetAccountResult, StoredAccountData> {
        return TransformOperation<SetAccountResult, StoredAccountData>(dispatchQueue: dispatchQueue) { input in
            self.logger.debug("Saving settings...")

            let device = input.device
            let newDeviceState = DeviceState.loggedIn(
                input.accountData,
                StoredDeviceData(
                    creationDate: device.created,
                    identifier: device.id,
                    name: device.name,
                    hijackDNS: device.hijackDNS,
                    ipv4Address: device.ipv4Address,
                    ipv6Address: device.ipv6Address,
                    wgKeyData: StoredWgKeyData(
                        creationDate: Date(),
                        lastRotationAttemptDate: nil,
                        privateKey: input.privateKey
                    )
                )
            )

            self.interactor.setSettings(TunnelSettingsV2(), persist: true)
            self.interactor.setDeviceState(newDeviceState, persist: true)

            return input.accountData
        }
    }
}
