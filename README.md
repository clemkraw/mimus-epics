<div align="center">
  <h1 align="center">MIMUS-EPICS</h1>
  <p align="center">
    <strong>Mirrored IOC Model for Ultra-Scale Simulation of EPICS Control Systems</strong>
  </p>
  <p align="center">
    <em>A self-calibrating PVAccess traffic generator for testing EPICS 7 clients at facility scale.</em>
  </p>
  <p align="center">
    <a href="#why">Why</a> •
    <a href="#how-it-works">How it works</a> •
    <a href="#fidelity">Fidelity</a> •
    <a href="#usage">Usage</a> •
    <a href="#accounting">Accounting</a> •
    <a href="#scope-and-limitations">Scope</a> •
    <a href="#status">Status</a>
  </p>
  <p align="center">
    <img src="https://img.shields.io/badge/language-Rust_2024-orange?style=flat-square&logo=rust&logoColor=white" alt="Rust">
    <img src="https://img.shields.io/badge/runtime-Tokio-blue?style=flat-square&logo=data:image/svg+xml;base64,PHN2ZyB4bWxucz0iaHR0cDovL3d3dy53My5vcmcvMjAwMC9zdmciIHdpZHRoPSIxMjAiIGhlaWdodD0iMTA4IiB2aWV3Qm94PSIwIDAgMTIwIDEwOCI%2BPHRpdGxlPk1hcmsgQ29weTwvdGl0bGU%2BPGcgZmlsbD0iI2ZmZiIgZmlsbC1ydWxlPSJldmVub2RkIj48cGF0aCBkPSJtMjkuODkzIDY4Ljc5NC0xLjY4Ny45NzQgMiAzLjQ2NCAxLjY4Ny0uOTc0IDEyLjIxNC03LjA1MiAxLjY4Ny0uOTc0LTItMy40NjQtMS42ODcuOTc0em01OC4yMTQgMy40NjQgMS42ODcuOTc0IDItMy40NjQtMS42ODctLjk3NC0xMi4yMTQtNy4wNTItMS42ODctLjk3NC0yIDMuNDY0IDEuNjg3Ljk3NHoiLz48cGF0aCBkPSJNODAgNTRjMC0xMS4wNDYtOC45NTQtMjAtMjAtMjBzLTIwIDguOTU0LTIwIDIwIDguOTU0IDIwIDIwIDIwIDIwLTguOTU0IDIwLTIwbS0zNS43NjcgMGMwLTguNzA4IDcuMDU5LTE1Ljc2NyAxNS43NjctMTUuNzY3Uzc1Ljc2NyA0NS4yOTIgNzUuNzY3IDU0IDY4LjcwOCA2OS43NjcgNjAgNjkuNzY3IDQ0LjIzMyA2Mi43MDggNDQuMjMzIDU0Ii8%2BPGNpcmNsZSBjeD0iMjQiIGN5PSI3NSIgcj0iMyIvPjxjaXJjbGUgY3g9IjYwIiBjeT0iOTYiIHI9IjMiLz48Y2lyY2xlIGN4PSI2MCIgY3k9IjEyIiByPSIzIi8%2BPGNpcmNsZSBjeD0iOTYiIGN5PSIzMyIgcj0iMyIvPjxjaXJjbGUgY3g9IjI0IiBjeT0iMzMiIHI9IjMiLz48Y2lyY2xlIGN4PSI5NiIgY3k9Ijc1IiByPSIzIi8%2BPGNpcmNsZSBjeD0iNjAiIGN5PSI1NCIgcj0iMyIvPjxwYXRoIGQ9Ik0yIDUySDB2NGg0MnYtNGgtMnptNzggMGgtMnY0aDQydi00aC0yem0tNy4xNyAxOC4zODktMS4wMDctMS43MjktMy40NTYgMi4wMTQgMS4wMDcgMS43MjggMTkuMTI4IDMyLjgzNSAxLjAwNyAxLjcyOCAzLjQ1Ni0yLjAxNC0xLjAwNy0xLjcyOHpNMzIuNTgxIDIuMTQgMzEuNTc1LjQxMmwtMy40NTcgMi4wMTMgMS4wMDcgMS43MjkgMTkuMTI4IDMyLjgzNCAxLjAwNyAxLjcyOCAzLjQ1Ni0yLjAxMy0xLjAwNi0xLjcyOHpNOTAuNzEgNC4xNTRsMS4wMDYtMS43MjlMODguMjYuNDEyIDg3LjI1MyAyLjE0IDY4LjEyNSAzNC45NzVsLTEuMDA3IDEuNzI4IDMuNDU3IDIuMDEzIDEuMDA2LTEuNzI4ek01MC44MjcgNzIuNTk3bDEuMDA2LTEuNzI4LTMuNDU2LTIuMDE0LTEuMDA3IDEuNzI4LTE5LjEyOCAzMi44MzUtMS4wMDcgMS43MjggMy40NTcgMi4wMTQgMS4wMDYtMS43Mjh6TTU4IDg3LjAxN1Y4OWg0VjcxaC00djEuOTgzem0wLTUxLjk2NVYzN2g0VjE5aC00djEuOTQ4ek00Mi4xMDcgNDYuMjU4bDEuNjg3Ljk3NCAyLTMuNDY0LTEuNjg3LS45NzQtMTIuMjE0LTcuMDUyLTEuNjg3LS45NzQtMiAzLjQ2NCAxLjY4Ny45NzR6bTMzLjc4Ni0zLjQ2NC0xLjY4Ny45NzQgMiAzLjQ2NCAxLjY4Ny0uOTc0IDEyLjIxNC03LjA1MiAxLjY4Ny0uOTc0LTItMy40NjQtMS42ODcuOTc0eiIvPjwvZz48L3N2Zz4%3D" alt="Tokio">
    <img src="https://img.shields.io/badge/protocol-PVAccess_(pvxs)-blueviolet?style=flat-square&logo=data:image/svg+xml;base64,PHN2ZyB4bWxucz0iaHR0cDovL3d3dy53My5vcmcvMjAwMC9zdmciIHZpZXdCb3g9IjAgMCAyNCAyNCIgZmlsbD0ibm9uZSIgc3Ryb2tlPSIjZmZmIiBzdHJva2Utd2lkdGg9IjIuMyIgc3Ryb2tlLWxpbmVjYXA9InJvdW5kIiBzdHJva2UtbGluZWpvaW49InJvdW5kIj48cGF0aCBkPSJNMS41IDEyaDRsMy03LjUgNCAxNSAzLTcuNWg0Ii8%2BPC9zdmc%2BCg%3D%3D" alt="PVAccess">
    <img src="https://img.shields.io/badge/license-MIT-lightgrey?style=flat-square&logo=opensourceinitiative&logoColor=white" alt="License">
  </p>
</div>

> **A standalone PVAccess traffic generator. EPICS base is not required to build or run MIMUS** — it is a single Rust binary, not an EPICS module. A reference `softIocPVX` is needed once, during the calibration phase, to teach MIMUS what to emit.