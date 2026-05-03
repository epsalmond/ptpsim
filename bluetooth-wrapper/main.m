#import <Foundation/Foundation.h>
#import <IOBluetooth/IOBluetooth.h>
#import <objc/message.h>
#import <objc/runtime.h>
#import <dlfcn.h>
#import <fcntl.h>
#import <unistd.h>

static const uint64_t kBTSettingsDiscoveryFlags = 0x80000a00000ULL;

typedef id (*MsgSendId)(id, SEL);
typedef id (*MsgSendAlloc)(Class, SEL);
typedef id (*MsgSendInit)(id, SEL);
typedef id (*MsgSendIdWithId)(id, SEL, id);
typedef void (*MsgSendVoid)(id, SEL);
typedef void (*MsgSendVoidWithId)(id, SEL, id);
typedef void (*MsgSendActivate)(id, SEL, void (^)(NSError *));
typedef NSArray *(*MsgSendDevicesWithFlags)(Class, SEL, uint64_t, NSError **);
typedef void (*MsgSendDeleteDevice)(id, SEL, id, void (^)(NSError *));
typedef IOReturn (*MsgSendDeleteStoredLinkKey)(id, SEL, const BluetoothDeviceAddress *, uint8_t, uint16_t *);
typedef BOOL (*MsgSendBool)(id, SEL);
typedef long long (*MsgSendLongLong)(id, SEL);
typedef uint64_t (*MsgSendUInt64)(id, SEL);

static const char *SkipObjCTypeQualifiers(const char *type) {
    while (*type == 'r' || *type == 'n' || *type == 'N' || *type == 'o' ||
           *type == 'O' || *type == 'R' || *type == 'V') {
        type++;
    }
    return type;
}

static BOOL SelectorReturnsObject(id object, SEL selector) {
    Method method = class_getInstanceMethod([object class], selector);
    if (!method) {
        return NO;
    }

    char returnType[16] = {0};
    method_getReturnType(method, returnType, sizeof(returnType));
    const char *type = SkipObjCTypeQualifiers(returnType);
    return type[0] == '@' || type[0] == '#';
}

static BOOL SelectorReturnsInteger(id object, SEL selector) {
    Method method = class_getInstanceMethod([object class], selector);
    if (!method) {
        return NO;
    }

    char returnType[16] = {0};
    method_getReturnType(method, returnType, sizeof(returnType));
    const char *type = SkipObjCTypeQualifiers(returnType);
    switch (type[0]) {
        case 'B':
        case 'c':
        case 'C':
        case 's':
        case 'S':
        case 'i':
        case 'I':
        case 'l':
        case 'L':
        case 'q':
        case 'Q':
            return YES;
        default:
            return NO;
    }
}

static id SendId(id object, SEL selector) {
    if (!object || ![object respondsToSelector:selector]) {
        return nil;
    }
    if (!SelectorReturnsObject(object, selector)) {
        return nil;
    }
    return ((MsgSendId)objc_msgSend)(object, selector);
}

static BOOL SendBool(id object, SEL selector, BOOL fallback) {
    if (!object || ![object respondsToSelector:selector]) {
        return fallback;
    }
    if (!SelectorReturnsInteger(object, selector)) {
        return fallback;
    }
    return ((MsgSendBool)objc_msgSend)(object, selector);
}

static uint64_t SendUInt64(id object, SEL selector, uint64_t fallback) {
    if (!object || ![object respondsToSelector:selector]) {
        return fallback;
    }
    if (!SelectorReturnsInteger(object, selector)) {
        return fallback;
    }
    return ((MsgSendUInt64)objc_msgSend)(object, selector);
}

static long long SendLongLong(id object, SEL selector, long long fallback) {
    if (!object || ![object respondsToSelector:selector]) {
        return fallback;
    }
    if (!SelectorReturnsInteger(object, selector)) {
        return fallback;
    }
    return ((MsgSendLongLong)objc_msgSend)(object, selector);
}

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
    if ([value isKindOfClass:[NSData class]]) {
        const unsigned char *bytes = [value bytes];
        NSMutableArray<NSString *> *parts = [NSMutableArray arrayWithCapacity:[value length]];
        for (NSUInteger i = 0; i < [value length]; i++) {
            [parts addObject:[NSString stringWithFormat:@"%02X", bytes[i]]];
        }
        return [parts componentsJoinedByString:@":"];
    }
    return [value description];
}

static NSString *FirstStringForSelectors(id object, NSArray<NSString *> *selectorNames) {
    for (NSString *selectorName in selectorNames) {
        SEL selector = NSSelectorFromString(selectorName);
        id value = SendId(object, selector);
        NSString *string = StringFromValue(value);
        if ([string length] > 0) {
            return string;
        }
    }
    return nil;
}

static NSString *DeviceName(id device) {
    NSString *name = FirstStringForSelectors(device, @[
        @"name",
        @"bleName",
        @"displayName",
        @"model",
        @"productName"
    ]);
    return name ?: @"<unknown>";
}

static NSString *DeviceID(id device) {
    NSString *identifier = FirstStringForSelectors(device, @[
        @"identifier",
        @"address",
        @"addressString",
        @"bluetoothAddress",
        @"deviceAddress",
        @"hardwareAddressData",
        @"addressData",
        @"stableIdentifier"
    ]);
    if ([identifier length] > 0) {
        return identifier;
    }
    return [NSString stringWithFormat:@"%p", device];
}

static NSString *DeviceKey(id device) {
    NSString *identifier = DeviceID(device);
    if ([identifier length] > 0) {
        return [@"id:" stringByAppendingString:identifier];
    }
    return [NSString stringWithFormat:@"ptr:%p", device];
}

static BOOL DeviceLooksPaired(id device) {
    if ([device respondsToSelector:NSSelectorFromString(@"paired")]) {
        return SendBool(device, NSSelectorFromString(@"paired"), NO);
    }
    if ([device respondsToSelector:NSSelectorFromString(@"isPaired")]) {
        return SendBool(device, NSSelectorFromString(@"isPaired"), NO);
    }

    SEL deviceFlagsSelector = NSSelectorFromString(@"deviceFlags");
    if ([device respondsToSelector:deviceFlagsSelector]) {
        uint64_t flags = SendUInt64(device, deviceFlagsSelector, 0);
        return (flags & 0x1) != 0;
    }
    return NO;
}

static BOOL DeviceLooksConnected(id device) {
    for (NSString *selectorName in @[@"connected", @"isConnected"]) {
        SEL selector = NSSelectorFromString(selectorName);
        if ([device respondsToSelector:selector]) {
            return SendBool(device, selector, NO);
        }
    }
    if ([device respondsToSelector:NSSelectorFromString(@"connectionFlags")]) {
        long long flags = SendLongLong(device, NSSelectorFromString(@"connectionFlags"), 0);
        return flags != 0;
    }
    return NO;
}

static void AddDevice(NSMutableDictionary<NSString *, id> *devicesByKey, id device) {
    if (!device) {
        return;
    }
    devicesByKey[DeviceKey(device)] = device;
}

static void WithSuppressedStderr(void (^block)(void)) {
    int savedStderr = dup(STDERR_FILENO);
    int nullFd = open("/dev/null", O_WRONLY);
    if (savedStderr < 0 || nullFd < 0) {
        if (nullFd >= 0) {
            close(nullFd);
        }
        if (savedStderr >= 0) {
            close(savedStderr);
        }
        block();
        return;
    }

    fflush(stderr);
    dup2(nullFd, STDERR_FILENO);
    close(nullFd);
    @autoreleasepool {
        block();
    }
    fflush(stderr);
    dup2(savedStderr, STDERR_FILENO);
    close(savedStderr);
}

static void AddIOBluetoothDevices(NSMutableDictionary<NSString *, id> *devicesByKey) {
    WithSuppressedStderr(^{
        for (id device in [IOBluetoothDevice pairedDevices]) {
            AddDevice(devicesByKey, device);
        }
        for (id device in [IOBluetoothDevice recentDevices:20]) {
            AddDevice(devicesByKey, device);
        }
    });
}

static BOOL LoadBluetoothManagerFramework(void) {
    static BOOL loaded = NO;
    static dispatch_once_t onceToken;
    dispatch_once(&onceToken, ^{
        loaded = dlopen("/System/Library/PrivateFrameworks/BluetoothManager.framework/BluetoothManager",
                        RTLD_LAZY | RTLD_GLOBAL) != NULL;
    });
    return loaded;
}

static id BluetoothManagerSharedInstance(void) {
    if (!LoadBluetoothManagerFramework()) {
        return nil;
    }

    Class managerClass = NSClassFromString(@"BluetoothManager");
    SEL selector = NSSelectorFromString(@"sharedInstance");
    if (!managerClass || ![managerClass respondsToSelector:selector]) {
        return nil;
    }

    return ((MsgSendAlloc)objc_msgSend)(managerClass, selector);
}

static void AddBluetoothManagerDevices(NSMutableDictionary<NSString *, id> *devicesByKey) {
    id manager = BluetoothManagerSharedInstance();
    NSArray *pairedDevices = SendId(manager, NSSelectorFromString(@"pairedDevices"));
    if (![pairedDevices isKindOfClass:[NSArray class]]) {
        return;
    }

    for (id device in pairedDevices) {
        AddDevice(devicesByKey, device);
    }
}

static id BluetoothManagerDeviceForIdentifier(NSString *identifier) {
    if ([identifier length] == 0) {
        return nil;
    }

    id manager = BluetoothManagerSharedInstance();
    SEL addressSelector = NSSelectorFromString(@"deviceFromAddressString:");
    if (manager && [manager respondsToSelector:addressSelector]) {
        NSString *colonIdentifier = [[identifier stringByReplacingOccurrencesOfString:@"-" withString:@":"] uppercaseString];
        NSString *upperIdentifier = [identifier uppercaseString];
        NSArray<NSString *> *variants = @[identifier, upperIdentifier, colonIdentifier, [colonIdentifier lowercaseString]];
        NSMutableSet<NSString *> *seen = [NSMutableSet set];
        for (NSString *variant in variants) {
            if ([seen containsObject:variant]) {
                continue;
            }
            [seen addObject:variant];
            @try {
                id device = ((MsgSendIdWithId)objc_msgSend)(manager, addressSelector, variant);
                if (device) {
                    return device;
                }
            } @catch (NSException *exception) {
                (void)exception;
            }
        }
    }

    SEL identifierSelector = NSSelectorFromString(@"deviceFromIdentifier:");
    NSUUID *uuid = [[NSUUID alloc] initWithUUIDString:identifier];
    if (uuid && manager && [manager respondsToSelector:identifierSelector]) {
        @try {
            id device = ((MsgSendIdWithId)objc_msgSend)(manager, identifierSelector, uuid);
            if (device) {
                return device;
            }
        } @catch (NSException *exception) {
            (void)exception;
        }
    }

    return nil;
}

static NSArray *StaticDevices(Class discoveryClass, NSError **error) {
    SEL selector = NSSelectorFromString(@"devicesWithDiscoveryFlags:error:");
    if (![discoveryClass respondsToSelector:selector]) {
        return @[];
    }
    NSArray *devices = ((MsgSendDevicesWithFlags)objc_msgSend)(discoveryClass, selector, kBTSettingsDiscoveryFlags, error);
    return [devices isKindOfClass:[NSArray class]] ? devices : @[];
}

static NSArray *SortedDevices(NSMutableDictionary<NSString *, id> *devicesByKey) {
    NSArray *devices = nil;
    @synchronized (devicesByKey) {
        devices = [devicesByKey.allValues copy];
    }
    return [devices sortedArrayUsingComparator:^NSComparisonResult(id left, id right) {
        return [DeviceName(left) localizedCaseInsensitiveCompare:DeviceName(right)];
    }];
}

static NSArray *KnownDevices(NSError **staticDiscoveryError, BOOL includeIOBluetoothFallback) {
    NSMutableDictionary<NSString *, id> *devicesByKey = [NSMutableDictionary dictionary];
    NSError *staticError = nil;

    Class discoveryClass = NSClassFromString(@"CBDiscovery");
    if (discoveryClass) {
        for (id device in StaticDevices(discoveryClass, &staticError)) {
            AddDevice(devicesByKey, device);
        }
    }

    if (includeIOBluetoothFallback) {
        AddIOBluetoothDevices(devicesByKey);
    }
    AddBluetoothManagerDevices(devicesByKey);

    if (staticDiscoveryError && staticError && [devicesByKey count] == 0) {
        *staticDiscoveryError = staticError;
    }

    return SortedDevices(devicesByKey);
}

static void PrintDevice(id device) {
    printf("%-38s  %-32s  paired=%s connected=%s  %s\n",
           [DeviceID(device) UTF8String],
           [DeviceName(device) UTF8String],
           DeviceLooksPaired(device) ? "yes" : "no",
           DeviceLooksConnected(device) ? "yes" : "no",
           [[device description] UTF8String]);
}

static int ListDevices(void) {
    NSError *error = nil;
    NSArray *devices = KnownDevices(&error, YES);
    if (error) {
        fprintf(stderr, "warning: CoreBluetooth device lookup returned: %s\n", [[error localizedDescription] UTF8String]);
    }
    for (id device in devices) {
        PrintDevice(device);
    }
    return 0;
}

static NSArray *MatchingDevices(NSArray *devices, NSString *query) {
    NSMutableArray *matches = [NSMutableArray array];
    NSString *foldedQuery = [query lowercaseString];
    for (id device in devices) {
        NSString *identifier = [DeviceID(device) lowercaseString];
        NSString *name = [DeviceName(device) lowercaseString];
        if ([identifier isEqualToString:foldedQuery] || [name isEqualToString:foldedQuery]) {
            [matches addObject:device];
        }
    }
    if ([matches count] > 0) {
        return matches;
    }
    for (id device in devices) {
        NSString *identifier = [DeviceID(device) lowercaseString];
        NSString *name = [DeviceName(device) lowercaseString];
        if ([identifier containsString:foldedQuery] ||
            [name containsString:foldedQuery] ||
            [foldedQuery containsString:identifier] ||
            [foldedQuery containsString:name]) {
            [matches addObject:device];
        }
    }
    return matches;
}

static BOOL IOBluetoothPairedDeviceExists(NSString *identifier) {
    NSString *foldedIdentifier = [identifier lowercaseString];
    if ([foldedIdentifier length] == 0) {
        return NO;
    }

    __block BOOL exists = NO;
    WithSuppressedStderr(^{
        for (id device in [IOBluetoothDevice pairedDevices]) {
            if ([[DeviceID(device) lowercaseString] isEqualToString:foldedIdentifier]) {
                exists = YES;
                break;
            }
        }
    });
    return exists;
}

static BOOL WaitForIOBluetoothDeviceToDisappear(NSString *identifier, NSTimeInterval timeoutSeconds) {
    NSDate *until = [NSDate dateWithTimeIntervalSinceNow:timeoutSeconds];
    while ([until timeIntervalSinceNow] > 0) {
        if (!IOBluetoothPairedDeviceExists(identifier)) {
            return YES;
        }
        [[NSRunLoop currentRunLoop] runMode:NSDefaultRunLoopMode beforeDate:[NSDate dateWithTimeIntervalSinceNow:0.1]];
    }
    return !IOBluetoothPairedDeviceExists(identifier);
}

static int ForgetBluetoothManagerDevice(id device) {
    NSString *name = DeviceName(device);
    NSString *identifier = DeviceID(device);
    id manager = BluetoothManagerSharedInstance();
    BOOL requested = NO;

    SEL unpairDeviceSelector = NSSelectorFromString(@"unpairDevice:");
    if (manager && [manager respondsToSelector:unpairDeviceSelector]) {
        ((MsgSendVoidWithId)objc_msgSend)(manager, unpairDeviceSelector, device);
        requested = YES;
    }

    SEL unpairSelector = NSSelectorFromString(@"unpair");
    if ([device respondsToSelector:unpairSelector]) {
        ((MsgSendVoid)objc_msgSend)(device, unpairSelector);
        requested = YES;
    }

    SEL removeDeviceSelector = NSSelectorFromString(@"_removeDevice:");
    if (manager && [manager respondsToSelector:removeDeviceSelector]) {
        ((MsgSendVoidWithId)objc_msgSend)(manager, removeDeviceSelector, device);
        requested = YES;
    }

    if (!requested) {
        fprintf(stderr, "BluetoothManager does not provide an unpair selector for %s (%s)\n",
                [name UTF8String],
                [identifier UTF8String]);
        return 1;
    }

    if (WaitForIOBluetoothDeviceToDisappear(identifier, 2.0)) {
        printf("forgot %s (%s)\n", [name UTF8String], [identifier UTF8String]);
        return 0;
    }

    fprintf(stderr, "forget was requested for %s (%s), but it still appears in paired devices\n",
            [name UTF8String],
            [identifier UTF8String]);
    fprintf(stderr, "BluetoothManager unpair APIs did not clear this paired record on this macOS build.\n");
    return 1;
}

static int ForgetIOBluetoothDevice(id device) {
    NSString *name = DeviceName(device);
    NSString *identifier = DeviceID(device);
    BOOL requested = NO;

    SEL removeFromFavoritesSelector = NSSelectorFromString(@"removeFromFavorites");
    if ([device respondsToSelector:removeFromFavoritesSelector]) {
        ((MsgSendLongLong)objc_msgSend)(device, removeFromFavoritesSelector);
    }

    SEL removeLinkKeySelector = NSSelectorFromString(@"removeLinkKey");
    if ([device respondsToSelector:removeLinkKeySelector]) {
        ((MsgSendVoid)objc_msgSend)(device, removeLinkKeySelector);
        requested = YES;
    }

    Class hostControllerClass = NSClassFromString(@"IOBluetoothHostController");
    SEL defaultControllerSelector = NSSelectorFromString(@"defaultController");
    SEL deleteStoredLinkKeySelector = NSSelectorFromString(@"BluetoothHCIDeleteStoredLinkKey:inDeleteAllFlag:outNumKeysDeleted:");
    if (hostControllerClass && [hostControllerClass respondsToSelector:defaultControllerSelector]) {
        id controller = ((MsgSendAlloc)objc_msgSend)(hostControllerClass, defaultControllerSelector);
        if (controller && [controller respondsToSelector:deleteStoredLinkKeySelector]) {
            const BluetoothDeviceAddress *address = [(IOBluetoothDevice *)device getAddress];
            if (address) {
                uint16_t deletedKeys = 0;
                IOReturn result = ((MsgSendDeleteStoredLinkKey)objc_msgSend)(controller,
                                                                             deleteStoredLinkKeySelector,
                                                                             address,
                                                                             0,
                                                                             &deletedKeys);
                if (result == kIOReturnSuccess || deletedKeys > 0) {
                    requested = YES;
                }
            }
        }
    }

    SEL removeSelector = NSSelectorFromString(@"remove");
    if ([device respondsToSelector:removeSelector]) {
        ((MsgSendVoid)objc_msgSend)(device, removeSelector);
        requested = YES;
    }

    SEL forceRemoveSelector = NSSelectorFromString(@"forceRemove");
    if ([device respondsToSelector:forceRemoveSelector]) {
        ((MsgSendVoid)objc_msgSend)(device, forceRemoveSelector);
        requested = YES;
    }

    if (!requested) {
        fprintf(stderr, "IOBluetoothDevice does not provide a usable removal selector for %s (%s)\n",
                [name UTF8String],
                [identifier UTF8String]);
        return 1;
    }

    if (WaitForIOBluetoothDeviceToDisappear(identifier, 2.0)) {
        printf("forgot %s (%s)\n", [name UTF8String], [identifier UTF8String]);
        return 0;
    }

    fprintf(stderr, "forget was requested for %s (%s), but it still appears in paired devices\n",
            [name UTF8String],
            [identifier UTF8String]);
    fprintf(stderr, "IOBluetooth removal APIs did not clear this paired record on this macOS build.\n");
    return 1;
}

static int ForgetCoreBluetoothDevice(id device) {
    Class controllerClass = NSClassFromString(@"CBController");
    if (!controllerClass) {
        fprintf(stderr, "CBController is unavailable. CoreBluetooth private SPI did not load.\n");
        return 1;
    }

    id controller = ((MsgSendInit)objc_msgSend)(((MsgSendAlloc)objc_msgSend)(controllerClass, @selector(alloc)), @selector(init));
    if (!controller) {
        fprintf(stderr, "failed to create CBController\n");
        return 1;
    }

    if ([controller respondsToSelector:NSSelectorFromString(@"activateWithCompletion:")]) {
        __block BOOL activated = NO;
        __block NSError *activateError = nil;
        ((MsgSendActivate)objc_msgSend)(controller, NSSelectorFromString(@"activateWithCompletion:"), ^(NSError *blockError) {
            activateError = blockError;
            activated = YES;
        });
        BOOL completed = RunLoopUntil(^BOOL{
            return activated;
        }, 5.0);
        if (!completed) {
            fprintf(stderr, "controller activation timed out\n");
            return 1;
        }
        if (activateError) {
            fprintf(stderr, "controller activation failed: %s\n", [[activateError localizedDescription] UTF8String]);
            return 1;
        }
    }

    SEL deleteSelector = NSSelectorFromString(@"deleteDevice:completion:");
    if (![controller respondsToSelector:deleteSelector]) {
        fprintf(stderr, "CBController does not respond to deleteDevice:completion:\n");
        return 1;
    }

    __block BOOL finished = NO;
    __block NSError *deleteError = nil;
    ((MsgSendDeleteDevice)objc_msgSend)(controller, deleteSelector, device, ^(NSError *blockError) {
        deleteError = blockError;
        finished = YES;
    });

    BOOL completed = RunLoopUntil(^BOOL{
        return finished;
    }, 15.0);
    if (!completed) {
        fprintf(stderr, "forget timed out for %s (%s)\n", [DeviceName(device) UTF8String], [DeviceID(device) UTF8String]);
        return 1;
    }
    if (deleteError) {
        fprintf(stderr, "forget failed for %s (%s): %s\n",
                [DeviceName(device) UTF8String],
                [DeviceID(device) UTF8String],
                [[deleteError localizedDescription] UTF8String]);
        return 1;
    }

    printf("forgot %s (%s)\n", [DeviceName(device) UTF8String], [DeviceID(device) UTF8String]);
    return 0;
}

static int ForgetDevice(NSString *query) {
    NSError *error = nil;
    NSArray *devices = KnownDevices(&error, YES);
    if (error) {
        fprintf(stderr, "warning: CoreBluetooth device lookup returned: %s\n", [[error localizedDescription] UTF8String]);
    }

    NSArray *matches = MatchingDevices(devices, query);
    if ([matches count] == 0) {
        fprintf(stderr, "no device matched '%s'\n", [query UTF8String]);
        fprintf(stderr, "examples:\n");
        fprintf(stderr, "  ./bluetooth-wrapper forget 'GFX100 II'\n");
        fprintf(stderr, "  ./bluetooth-wrapper forget 38-7c-76-74-73-21\n");
        return 2;
    }
    if ([matches count] > 1) {
        fprintf(stderr, "multiple devices matched '%s'; use a more specific name or id:\n", [query UTF8String]);
        for (id device in matches) {
            fprintf(stderr, "  %s  %s\n", [DeviceID(device) UTF8String], [DeviceName(device) UTF8String]);
        }
        return 2;
    }

    id device = [matches firstObject];
    Class bluetoothDeviceClass = NSClassFromString(@"BluetoothDevice");
    if (bluetoothDeviceClass && [device isKindOfClass:bluetoothDeviceClass]) {
        return ForgetBluetoothManagerDevice(device);
    }
    if ([device isKindOfClass:[IOBluetoothDevice class]]) {
        id bluetoothManagerDevice = BluetoothManagerDeviceForIdentifier(DeviceID(device));
        if (bluetoothManagerDevice) {
            int result = ForgetBluetoothManagerDevice(bluetoothManagerDevice);
            if (result == 0) {
                return 0;
            }
        }
        return ForgetIOBluetoothDevice(device);
    }
    return ForgetCoreBluetoothDevice(device);
}

static void PrintUsage(const char *argv0) {
    fprintf(stderr, "usage:\n");
    fprintf(stderr, "  %s [list]\n", argv0);
    fprintf(stderr, "  %s forget <name-or-id>\n", argv0);
    fprintf(stderr, "\nexamples:\n");
    fprintf(stderr, "  %s forget 'GFX100 II'\n", argv0);
    fprintf(stderr, "  %s forget 38-7c-76-74-73-21\n", argv0);
}

int main(int argc, const char *argv[]) {
    @autoreleasepool {
        NSString *verb = argc >= 2 ? [NSString stringWithUTF8String:argv[1]] : @"list";

        if ([verb isEqualToString:@"list"]) {
            if (argc > 2) {
                PrintUsage(argv[0]);
                return 64;
            }
            return ListDevices();
        }

        if ([verb isEqualToString:@"forget"] || [verb isEqualToString:@"delete"]) {
            if (argc != 3) {
                PrintUsage(argv[0]);
                return 64;
            }
            return ForgetDevice([NSString stringWithUTF8String:argv[2]]);
        }

        PrintUsage(argv[0]);
        return 64;
    }
}
