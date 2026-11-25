# Builds mimus-ndarray, the reference server for NTNDArray images and large
# waveforms, against an EPICS base and a PVXS installation:
#
#   make EPICS_BASE=/path/to/base PVXS=/path/to/pvxs
#
# The defaults are common install locations; give your own if they differ.

EPICS_BASE ?= /opt/epics/base
PVXS ?= /opt/epics/pvxs
EPICS_HOST_ARCH ?= linux-x86_64

CXXFLAGS ?= -O2 -g -Wall -Wextra
CPPFLAGS += -I$(PVXS)/include -I$(EPICS_BASE)/include \
	-I$(EPICS_BASE)/include/os/Linux -I$(EPICS_BASE)/include/compiler/gcc
LIBDIRS = $(PVXS)/lib/$(EPICS_HOST_ARCH) $(EPICS_BASE)/lib/$(EPICS_HOST_ARCH)
LDFLAGS += $(foreach d,$(LIBDIRS),-L$(d) -Wl,-rpath,$(d)) -pthread
LDLIBS += -lpvxs -lCom

mimus-ndarray: mimus-ndarray.cpp
	$(CXX) $(CPPFLAGS) $(CXXFLAGS) -std=c++11 -o $@ $< $(LDFLAGS) $(LDLIBS)

clean:
	rm -f mimus-ndarray

.PHONY: clean
