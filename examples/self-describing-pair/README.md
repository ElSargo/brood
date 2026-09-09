# External package acceptance actor

The package behavior proof copies this source outside the checkout and adds
its exact WIT and portable package contracts with a locked build environment.
Its manifest builds daemon, session, and presentation artifacts from those
captured inputs. Applying the plan succeeds after the external source is deleted.

The paired actors register their implemented interface version, wait for a host
binding, and exchange a schema-validated ping/pong. The daemon actor exercises
root-owned lifecycle without providing the paired service. Fixture features
exercise incompatible presentation versions and failed replacement startup.
The proof inspects those outcomes through public package and control commands,
including disable/re-enable teardown, removal, and disconnected observation.
