# Copyright (c) 2022 NexToken Technology.
# SPDX-License-Identifier: Apache-2.0
#
# Licensed under the Apache License, Version 2.0 (the "License");
# you may not use this file except in compliance with the License.
# You may obtain a copy of the License at
#
#  http://www.apache.org/licenses/LICENSE-2.0
#
# Unless required by applicable law or agreed to in writing, software
# distributed under the License is distributed on an "AS IS" BASIS,
# WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
# See the License for the specific language governing permissions and
# limitations under the License.

FROM phusion/baseimage:0.11 as builder
LABEL maintainer="team@capsule.ink"
LABEL description="Capsule Archive Node Builder"

ARG PROFILE=release
ARG STABLE=nightly-2021-09-12
WORKDIR /rustbuilder
COPY . /rustbuilder/Capsule

# UPDATE RUST DEPENDENCIES
ENV RUSTUP_HOME "/rustbuilder/.rustup"
ENV CARGO_HOME "/rustbuilder/.cargo"
RUN curl -sSf https://sh.rustup.rs | sh -s -- --default-toolchain none -y
ENV PATH "$PATH:/rustbuilder/.cargo/bin"
RUN rustup update $STABLE