#import <Foundation/Foundation.h>
#import <CoreBluetooth/CoreBluetooth.h>
#import <IOBluetooth/IOBluetooth.h>
#import <objc/message.h>
#import <objc/runtime.h>

typedef id (*MsgId)(id, SEL);
typedef id (*MsgClass)(Class, SEL);
typedef void (*MsgSetId)(id, SEL, id);
typedef void (*MsgSetUInt64)(id, SEL, uint64_t);
typedef void (*MsgActivate)(id, SEL, void (^)(NSError *));
typedef void (*MsgDelete)(id, SEL, id, void (^)(NSError *));
typedef BOOL (*MsgBool)(id, SEL);

static BOOL RunLoopUntil(BOOL (^condition)(void), NSTimeInterval timeoutSeconds) {
    NSDate *until = [NSDate dateWithTimeIntervalSinceNow:timeoutSeconds];
    while (!condition() && [until timeIntervalSinceNow] > 0) {
        [[NSRunLoop currentRunLoop] runMode:NSDefaultRunLoopMode beforeDate:[NSDate dateWithTimeIntervalSinceNow:0.1]];
    }
    return condition();
}

static NSString *StringFromValue(id value) {
    if (!value || value == (id)kCFNull) {
        return nil;
    }
    if ([value isKindOfClass:[NSString class]]) {
        return value;
    }
    if ([value isKindOfClass:[NSUUID class]]) {
        return [value UUIDString];
    }
    return [value description];
}

static NSString *DeviceID(IOBluetoothDevice *device) {
    return [device addressString] ?: [NSString stringWithFormat:@"%p", device];
}

static IOBluetoothDevice *FindDevice(NSString *query) {
    NSString *foldedQuery = [query lowercaseString];
    for (IOBluetoothDevice *device in [IOBluetoothDevice pairedDevices]) {
        NSString *address = [[DeviceID(device) lowercaseString] stringByReplacingOccurrencesOfString:@":" withString:@"-"];
        NSString *name = [[device name] lowercaseString];
        if ([address isEqualToString:foldedQuery] || [name isEqualToString:foldedQuery]) {
            return device;
        }
    }
    return nil;
}

static BOOL DeviceExists(NSString *identifier) {
    NSString *foldedIdentifier = [[identifier lowercaseString] stringByReplacingOccurrencesOfString:@":" withString:@"-"];
    for (IOBluetoothDevice *device in [IOBluetoothDevice pairedDevices]) {
        NSString *address = [[[device addressString] lowercaseString] stringByReplacingOccurrencesOfString:@":" withString:@"-"];
        if ([address isEqualToString:foldedIdentifier]) {
            return YES;
        }
    }
    return NO;
}

static BOOL BoolSelector(id object, NSString *selectorName) {
    SEL selector = NSSelectorFromString(selectorName);
    if (![object respondsToSelector:selector]) {
        return NO;
    }
    return ((MsgBool)objc_msgSend)(object, selector);
}

static void PrintPairState(NSString *query) {
    IOBluetoothDevice *device = FindDevice(query);
    if (!device) {
        printf("device not found: %s\n", [query UTF8String]);
        return;
    }

    printf("device=%s\n", [[device description] UTF8String]);
    for (NSString *selectorName in @[
        @"isPaired",
        @"isBRPaired",
        @"isLEPaired",
        @"isMCPaired",
        @"isiCloudPaired",
        @"magicCloudPairedPaired",
        @"isFavorite",
        @"isRecent",
        @"isConnected",
        @"isLowEnergyDevice"
    ]) {
        printf("%s=%s\n", [selectorName UTF8String], BoolSelector(device, selectorName) ? "yes" : "no");
    }

    for (NSString *selectorName in @[@"classicPeer", @"peer", @"peripheral"]) {
        SEL selector = NSSelectorFromString(selectorName);
        if (![device respondsToSelector:selector]) {
            continue;
        }
        id value = ((MsgId)objc_msgSend)(device, selector);
        printf("%s=%s class=%s\n",
               [selectorName UTF8String],
               [[StringFromValue(value) ?: @"(null)" description] UTF8String],
               value ? class_getName([value class]) : "nil");
    }
}

static NSData *AddressData(IOBluetoothDevice *device, BOOL reversed) {
    const BluetoothDeviceAddress *address = [device getAddress];
    if (!address) {
        return nil;
    }

    uint8_t bytes[6] = {0};
    for (NSUInteger i = 0; i < sizeof(bytes); i++) {
        bytes[i] = reversed ? address->data[sizeof(bytes) - i - 1] : address->data[i];
    }
    return [NSData dataWithBytes:bytes length:sizeof(bytes)];
}

static id ConstructCBDevice(IOBluetoothDevice *device, BOOL reversedAddress) {
    Class cbDeviceClass = NSClassFromString(@"CBDevice");
    if (!cbDeviceClass) {
        return nil;
    }

    id cbDevice = ((MsgId)objc_msgSend)(((MsgClass)objc_msgSend)(cbDeviceClass, @selector(alloc)), @selector(init));
    if (!cbDevice) {
        return nil;
    }

    NSString *name = [device name] ?: [device addressString];
    ((MsgSetId)objc_msgSend)(cbDevice, NSSelectorFromString(@"setName:"), name);
    ((MsgSetId)objc_msgSend)(cbDevice, NSSelectorFromString(@"setProductName:"), name);

    SEL peripheralSelector = NSSelectorFromString(@"peripheral");
    if ([device respondsToSelector:peripheralSelector]) {
        id peripheral = ((MsgId)objc_msgSend)(device, peripheralSelector);
        SEL identifierSelector = NSSelectorFromString(@"identifier");
        if (peripheral && [peripheral respondsToSelector:identifierSelector]) {
            id identifier = ((MsgId)objc_msgSend)(peripheral, identifierSelector);
            if (identifier) {
                ((MsgSetId)objc_msgSend)(cbDevice, NSSelectorFromString(@"setIdentifier:"), StringFromValue(identifier));
            }
        }
    }

    NSData *addressData = AddressData(device, reversedAddress);
    if (addressData) {
        ((MsgSetId)objc_msgSend)(cbDevice, NSSelectorFromString(@"setBtAddressData:"), addressData);
        ((MsgSetId)objc_msgSend)(cbDevice, NSSelectorFromString(@"setBleAddressData:"), addressData);
    }
    ((MsgSetUInt64)objc_msgSend)(cbDevice, NSSelectorFromString(@"setDeviceFlags:"), 1ULL);
    return cbDevice;
}

static int TryConstructedDelete(NSString *query) {
    IOBluetoothDevice *device = FindDevice(query);
    if (!device) {
        fprintf(stderr, "device not found: %s\n", [query UTF8String]);
        return 2;
    }

    NSString *identifier = DeviceID(device);
    Class controllerClass = NSClassFromString(@"CBController");
    if (!controllerClass) {
        fprintf(stderr, "CBController is unavailable\n");
        return 1;
    }

    for (NSNumber *reversedAddressValue in @[@NO, @YES]) {
        id cbDevice = ConstructCBDevice(device, [reversedAddressValue boolValue]);
        if (!cbDevice) {
            continue;
        }

        printf("trying constructed CBDevice reversed=%s %s\n",
               [reversedAddressValue boolValue] ? "yes" : "no",
               [[cbDevice description] UTF8String]);

        id controller = ((MsgId)objc_msgSend)(((MsgClass)objc_msgSend)(controllerClass, @selector(alloc)), @selector(init));
        __block BOOL activated = NO;
        __block NSError *activateError = nil;
        ((MsgActivate)objc_msgSend)(controller, NSSelectorFromString(@"activateWithCompletion:"), ^(NSError *error) {
            activateError = error;
            activated = YES;
        });
        RunLoopUntil(^BOOL{
            return activated;
        }, 5.0);
        if (activateError) {
            printf("activateError=%s\n", [[activateError description] UTF8String]);
        }

        __block BOOL done = NO;
        __block NSError *deleteError = nil;
        ((MsgDelete)objc_msgSend)(controller, NSSelectorFromString(@"deleteDevice:completion:"), cbDevice, ^(NSError *error) {
            deleteError = error;
            done = YES;
        });
        BOOL completed = RunLoopUntil(^BOOL{
            return done;
        }, 15.0);
        printf("delete completed=%s error=%s\n", completed ? "yes" : "no", [[deleteError description] UTF8String]);

        NSDate *until = [NSDate dateWithTimeIntervalSinceNow:5];
        while ([until timeIntervalSinceNow] > 0 && DeviceExists(identifier)) {
            [[NSRunLoop currentRunLoop] runMode:NSDefaultRunLoopMode beforeDate:[NSDate dateWithTimeIntervalSinceNow:0.1]];
        }
        if (!DeviceExists(identifier)) {
            printf("forgot %s (%s)\n", [[device name] UTF8String], [identifier UTF8String]);
            return 0;
        }
    }

    fprintf(stderr, "constructed CBDevice delete did not remove %s (%s)\n",
            [[[device name] description] UTF8String],
            [identifier UTF8String]);
    return 1;
}

int main(int argc, const char *argv[]) {
    @autoreleasepool {
        if (argc < 3) {
            fprintf(stderr, "usage: bt_probe pair-state <name-or-id>\n");
            fprintf(stderr, "       bt_probe try-constructed-delete <name-or-id>\n");
            return 64;
        }

        NSString *mode = [NSString stringWithUTF8String:argv[1]];
        NSString *query = [NSString stringWithUTF8String:argv[2]];
        if ([mode isEqualToString:@"pair-state"]) {
            PrintPairState(query);
            return 0;
        }
        if ([mode isEqualToString:@"try-constructed-delete"]) {
            return TryConstructedDelete(query);
        }

        fprintf(stderr, "unknown probe mode: %s\n", argv[1]);
        return 64;
    }
}
