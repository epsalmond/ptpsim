CC := xcrun clang
TARGET := bluetooth-wrapper
SOURCES := main.m
CFLAGS := -fobjc-arc -Wall -Wextra -Werror -O2
LDFLAGS := -framework Foundation -framework CoreBluetooth -framework IOBluetooth

.PHONY: all clean

all: $(TARGET)

$(TARGET): $(SOURCES)
	$(CC) $(CFLAGS) $(SOURCES) $(LDFLAGS) -o $(TARGET)

clean:
	rm -f $(TARGET)
